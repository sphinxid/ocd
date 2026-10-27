//! Command registry (palette + slash, spec §5.10) and command execution.

use super::{App, Route, Toast};
use crate::api::types::*;
use crate::state::cycle_variant;
use crate::ui::dialog::{Action, Body, DIALOG_INSET, Dialog, Footer, FooterItem, Opt, PromptDialog, Select, Size, TextDialog};
use crate::util::locale::{self, titlecase};
use serde_json::{Value, json};

#[derive(Clone, Debug, Default)]
pub struct Cmd {
    pub id: String,
    pub title: String,
    pub group: &'static str,
    pub slash: Option<&'static str>,
    pub aliases: Vec<&'static str>,
    pub args: bool,
    pub palette: bool,
    pub suggested: bool,
    pub description: Option<String>,
}

fn cmd(id: &str, title: impl Into<String>, group: &'static str) -> Cmd {
    Cmd { id: id.into(), title: title.into(), group, palette: true, ..Default::default() }
}

impl Cmd {
    fn slash(mut self, s: &'static str) -> Cmd {
        self.slash = Some(s);
        self
    }
    fn aliases(mut self, a: &[&'static str]) -> Cmd {
        self.aliases = a.to_vec();
        self
    }
    fn args(mut self) -> Cmd {
        self.args = true;
        self
    }
    fn hidden(mut self) -> Cmd {
        self.palette = false;
        self
    }
    fn suggested(mut self, s: bool) -> Cmd {
        self.suggested = s;
        self
    }
}

/// Settings rows (`dialog-config.tsx`): (category, title, path, values, labels, default).
pub fn settings() -> Vec<(&'static str, &'static str, &'static str, Vec<Value>, Vec<&'static str>, Value)> {
    let onoff = || (vec![json!(false), json!(true)], vec!["off", "on"]);
    let mut v = Vec::new();
    v.push(("Appearance", "Theme", "theme.name", vec![], vec![], json!("opencode")));
    v.push(("Appearance", "Color mode", "theme.mode", vec![json!("system"), json!("dark"), json!("light")], vec![], json!("system")));
    let (a, b) = onoff();
    v.push(("Appearance", "Animations", "animations", a, b, json!(false)));
    v.push(("Session", "Sidebar", "session.sidebar", vec![json!("hide"), json!("auto")], vec![], json!("auto")));
    let (a, b) = onoff();
    v.push(("Session", "Scrollbar", "session.scrollbar", a, b, json!(false)));
    v.push(("Session", "Thinking", "session.thinking", vec![json!("hide"), json!("show")], vec![], json!("hide")));
    v.push(("Session", "Markdown", "session.markdown", vec![json!("source"), json!("rendered")], vec![], json!("rendered")));
    v.push(("Session", "Tool grouping", "session.grouping", vec![json!("none"), json!("auto")], vec![], json!("auto")));
    v.push(("Session", "Verbosity", "session.verbosity", vec![json!("low"), json!("medium"), json!("high")], vec![], json!("medium")));
    let (a, b) = onoff();
    v.push(("Session", "Transcript images", "session.image_preview", a, b, json!(false)));
    let (a, b) = onoff();
    v.push(("Session", "TPS", "session.tps", a, b, json!(true)));
    v.push(("Session", "New session location", "session.new_location", vec![json!("launch"), json!("inherit")], vec!["launch directory", "active session"], json!("launch")));
    v.push(("Session", "Permissions", "session.permissions", vec![json!("prompt"), json!("autoaccept")], vec!["prompt", "auto accept"], json!("prompt")));
    v.push(("Tabs", "Mode", "tabs.mode", vec![json!("off"), json!("on"), json!("auto")], vec![], json!("auto")));
    v.push(("Tabs", "Scope", "tabs.scope", vec![json!("cwd"), json!("global")], vec!["current directory", "global"], json!("cwd")));
    v.push(("Tabs", "Layout", "tabs.layout", vec![json!("horizontal"), json!("vertical")], vec![], json!("horizontal")));
    v.push(("Tabs", "Indicators", "tabs.indicators", vec![json!("status"), json!("numbers")], vec!["status icons", "always show numbers"], json!("status")));
    v.push(("Diffs", "Layout", "diffs.view", vec![json!("auto"), json!("split"), json!("unified")], vec![], json!("auto")));
    v.push(("Diffs", "Wrapping", "diffs.wrap", vec![json!("none"), json!("word")], vec![], json!("word")));
    let (a, b) = onoff();
    v.push(("Diffs", "File tree", "diffs.tree", a, b, json!(true)));
    let (a, b) = onoff();
    v.push(("Diffs", "Single patch", "diffs.single", a, b, json!(false)));
    v.push(("Input", "Scroll speed", "scroll.speed", vec![], vec![], json!(3)));
    let (a, b) = onoff();
    v.push(("Input", "Acceleration", "scroll.acceleration", a, b, json!(false)));
    let (a, b) = onoff();
    v.push(("Input", "Mouse", "mouse", a, b, json!(true)));
    let (a, b) = onoff();
    v.push(("Input", "Editor context", "prompt.editor", a, b, json!(true)));
    v.push(("Input", "Large pastes", "prompt.paste", vec![json!("compact"), json!("full")], vec![], json!("compact")));
    let (a, b) = onoff();
    v.push(("Input", "Image previews", "prompt.image_preview", a, b, json!(false)));
    v.push(("Input", "Leader timeout", "leader.timeout", vec![], vec![], json!(2000)));
    let (a, b) = onoff();
    v.push(("Alerts", "Notifications", "attention.notifications", a, b, json!(false)));
    let (a, b) = onoff();
    v.push(("Alerts", "Sounds", "attention.sound", a, b, json!(false)));
    v.push(("Alerts", "Volume", "attention.volume", vec![], vec![], json!(0.4)));
    let (a, b) = onoff();
    v.push(("Alerts", "Release highlights", "highlights", a, b, json!(true)));
    let (a, b) = onoff();
    v.push(("Terminal", "Window title", "terminal.title", a, b, json!(true)));
    v.push(("Terminal", "Copy behavior", "terminal.copy", vec![json!("manual"), json!("select")], vec![], json!(if cfg!(windows) { "manual" } else { "select" })));
    let (a, b) = onoff();
    v.push(("Debug", "Developer tools", "debug.devtools", a, b, json!(false)));
    v
}

impl App {
    /// Reachable commands in palette order (session route, prompt, frame, app).
    pub fn commands(&self) -> Vec<Cmd> {
        let mut v: Vec<Cmd> = Vec::new();
        let in_session = self.session_id().is_some();
        let session = self.session();
        let has_revert = session.is_some_and(|s| s.revert.is_some());
        let tabs = self.config.tabs_enabled();
        // Registration order of the keymap layers: app, session route, prompt, session frame,
        // feature plugins. The palette groups by first appearance, keeping this order within groups.
        // App commands.
        v.push(cmd("command.palette.show", "Show command palette", "System").hidden());
        v.push(cmd("session.list", "Switch session", "Session").slash("sessions").aliases(&["resume", "continue"]).suggested(!self.store.sessions.is_empty()));
        v.push(cmd("session.new", "New session", "Session").slash("new").suggested(in_session));
        v.push(cmd("session.clear", "Clear session", "Session").slash("clear"));
        v.push(cmd("open.menu", "Open session or project", "Session").slash("open").aliases(&["projects", "project"]));
        if tabs {
            v.push(cmd("session.tab.close", "Close tab", "Session"));
            v.push(cmd("session.tab.reopen", "Reopen closed tab", "Session"));
            v.push(cmd("session.tab.next", "Next tab", "Session").hidden());
            v.push(cmd("session.tab.previous", "Previous tab", "Session").hidden());
            for i in 1..=10 {
                v.push(cmd(&format!("session.tab.select.{i}"), format!("Switch to tab {i}"), "Session").hidden());
            }
        } else {
            for i in 1..=9 {
                v.push(cmd(&format!("session.quick_switch.{i}"), format!("Switch to session in quick slot {i}"), "Session").hidden());
            }
        }
        v.push(cmd("model.list", "Switch model", "Agent").slash("models").suggested(true));
        v.push(cmd("model.cycle_recent", "Model cycle", "Agent").hidden());
        v.push(cmd("model.cycle_recent_reverse", "Model cycle reverse", "Agent").hidden());
        v.push(cmd("model.cycle_favorite", "Favorite cycle", "Agent").hidden());
        v.push(cmd("model.cycle_favorite_reverse", "Favorite cycle reverse", "Agent").hidden());
        v.push(cmd("agent.list", "Switch agent", "Agent").slash("agents"));
        v.push(cmd("mcp.list", "MCP servers", "Agent").slash("mcps"));
        v.push(cmd("agent.cycle", "Agent cycle", "Agent").hidden());
        v.push(cmd("variant.cycle", "Variant cycle", "Agent"));
        let has_variants = self.current_model_info().is_some_and(|m| !m.variants.is_empty());
        let mut variant = cmd("variant.list", "Switch model variant", "Agent").slash("variants").aliases(&["thinking", "effort"]);
        variant.palette = has_variants;
        v.push(variant);
        v.push(cmd("agent.cycle.reverse", "Agent cycle reverse", "Agent").hidden());
        let connected = self.has_connected_provider();
        v.push(cmd("provider.connect", "Connect an integration", "Integration").slash("connect").suggested(!connected));
        v.push(cmd("opencode.settings", "Open settings", "System").slash("settings").suggested(true));
        v.push(cmd("opencode.status", "View status", "System").slash("status"));
        v.push(cmd("server.pair", "Pair device", "System").slash("pair").aliases(&["web"]));
        if self.opts.service.is_some() {
            v.push(cmd("service.restart", "Restart service", "System").slash("restart"));
        }
        v.push(cmd("location.reload", "Reload configuration", "System").slash("reload"));
        v.push(cmd("opencode.debug", "View debug info", "System").slash("debug"));
        v.push(cmd("theme.switch", "Switch theme", "System").slash("themes"));
        let mode = self.themes.effective_mode();
        v.push(cmd("theme.switch_mode", if mode == crate::theme::Mode::Dark { "Switch to light mode" } else { "Switch to dark mode" }, "System").hidden());
        v.push(cmd("theme.mode.lock", if self.themes.lock.is_some() { "Unlock theme mode" } else { "Lock theme mode" }, "System").hidden());
        v.push(cmd("help.show", "Help", "System").slash("help"));
        v.push(cmd("docs.open", "Open docs", "System"));
        v.push(cmd("app.exit", "Exit the app", "System").slash("exit").aliases(&["quit", "q"]));
        v.push(cmd("app.console", "Toggle console", "System"));
        if !cfg!(windows) {
            v.push(cmd("terminal.suspend", "Suspend terminal", "System").hidden());
        }
        v.push(cmd("terminal.title.toggle", if self.config.terminal_title() { "Disable terminal title" } else { "Enable terminal title" }, "System").hidden());
        v.push(cmd("app.toggle.animations", if self.config.animations() { "Disable animations" } else { "Enable animations" }, "System").hidden());
        v.push(cmd("app.toggle.file_context", if self.config.prompt_editor() { "Disable file context" } else { "Enable file context" }, "System").hidden());
        v.push(cmd("app.toggle.diffwrap", if self.config.diffs_wrap() != "none" { "Disable diff wrapping" } else { "Enable diff wrapping" }, "System").hidden());
        v.push(cmd("app.toggle.paste_summary", if self.config.prompt_paste() != "full" { "Disable paste summary" } else { "Enable paste summary" }, "System").hidden());
        if in_session {
            v.push(cmd("session.rename", "Rename session", "Session").slash("rename").args());
            v.push(cmd("session.timeline", "Jump to message", "Session").slash("timeline"));
            v.push(cmd("session.fork", "Fork session", "Session").slash("fork"));
            v.push(cmd("session.compact", "Compact session", "Session").slash("compact"));
            v.push(cmd("session.undo", "Undo previous message", "Session").slash("undo"));
            if has_revert {
                v.push(cmd("session.redo", "Redo", "Session").slash("redo"));
            }
            let thinking = if self.config.thinking() == "show" { "Collapse thinking" } else { "Expand thinking" };
            v.push(cmd("session.toggle.thinking", thinking, "Session").hidden());
            v.push(cmd("session.toggle.scrollbar", "Toggle session scrollbar", "Session").hidden());
            let grouping = if self.config.grouping() == "none" { "Group related tool calls" } else { "Show tool calls individually" };
            v.push(cmd("session.toggle.exploration_grouping", grouping, "Session").hidden());
            v.push(cmd("session.verbosity.cycle", format!("Verbosity: {}", titlecase(self.config.verbosity())), "Session"));
            v.push(cmd("session.messages_last_user", "Jump to last user message", "Session").hidden());
            v.push(cmd("session.message.next", "Next message", "Session").hidden());
            v.push(cmd("session.message.previous", "Previous message", "Session").hidden());
            v.push(cmd("session.message.user.next", "Next user message", "Session").hidden());
            v.push(cmd("session.message.user.previous", "Previous user message", "Session").hidden());
            v.push(cmd("messages.copy", "Copy last assistant message", "Session").slash("copy"));
            v.push(cmd("session.copy.id", "Copy session ID", "Session"));
            v.push(cmd("session.copy", "Copy session transcript", "Session"));
            v.push(cmd("session.export", "Export session transcript", "Session").slash("export"));
            v.push(cmd("session.background", "Background blocking tools", "Session").hidden());
            v.push(cmd("session.child.first", "Toggle subagent picker", "Session"));
            if self.queued_prompts().len() > 0 {
                v.push(cmd("session.queued_prompts", "View queued prompts", "Prompt"));
            }
            if session.is_some_and(|s| s.parent_id.is_some()) {
                v.push(cmd("session.parent", "Go to parent session", "Session").hidden());
            }
            v.push(cmd("session.page.up", "Page up", "Session").hidden());
            v.push(cmd("session.page.down", "Page down", "Session").hidden());
            v.push(cmd("session.line.up", "Line up", "Session").hidden());
            v.push(cmd("session.line.down", "Line down", "Session").hidden());
            v.push(cmd("session.half.page.up", "Half page up", "Session").hidden());
            v.push(cmd("session.half.page.down", "Half page down", "Session").hidden());
            v.push(cmd("session.first", "First message", "Session").hidden());
            v.push(cmd("session.last", "Last message", "Session").hidden());
        }
        // Prompt commands.
        v.push(cmd("prompt.clear", "Clear prompt", "Prompt").hidden());
        v.push(cmd("prompt.submit", "Submit prompt", "Prompt").hidden());
        v.push(cmd("prompt.queue", "Queue prompt", "Prompt"));
        v.push(cmd("prompt.paste", "Paste", "Prompt").hidden());
        if self.running() {
            v.push(cmd("session.interrupt", "Interrupt session", "Session").hidden());
        }
        v.push(cmd("prompt.editor", "Open editor", "Session").slash("editor"));
        v.push(cmd("prompt.skills", "Skills", "Prompt").slash("skills"));
        v.push(cmd("session.move", "Manage workspaces", "Session").slash("worktrees"));
        if !self.editor.is_empty() {
            v.push(cmd("prompt.stash", "Stash prompt", "Prompt"));
        }
        if !self.stash.entries.is_empty() {
            v.push(cmd("prompt.stash.pop", "Stash pop", "Prompt"));
            v.push(cmd("prompt.stash.list", "Stash list", "Prompt"));
        }
        v.push(Cmd { id: "session.cd".into(), title: "Change working directory".into(), group: "", slash: Some("cd"), args: true, palette: false, ..Default::default() });
        // Session frame.
        if in_session {
            let open = self.sidebar_visible();
            v.push(cmd("session.sidebar.toggle", if open { "Hide sidebar" } else { "Show sidebar" }, "Session"));
            let right = self.terminal_visible().is_some() && self.active_panel().is_none();
            if right || self.active_panel().is_some() {
                v.push(Cmd { id: "pane.focus.left".into(), title: "Focus session pane".into(), ..Default::default() });
                v.push(Cmd { id: "pane.focus.right".into(), title: "Focus right pane".into(), ..Default::default() });
            }
            if self.terminals_available {
                v.push(cmd("terminal.toggle", if right { "Hide terminal pane" } else { "Show terminal pane" }, "Session"));
                v.push(cmd("terminal.select", "Select terminal", "Session"));
                if right {
                    v.push(cmd("terminal.close", "Close terminal pane", "Session"));
                }
                v.push(cmd("session.terminal", "New terminal", "Session").slash("terminal"));
            }
        }
        // Plugins (built-in).
        if self.builtin_active("opencode.btw") {
            v.push(Cmd { description: Some("One-shot answer from the session's context without adding to the conversation".into()), ..cmd("session.aside", "Ask a side question", "Session").slash("btw").args() });
        }
        v.push(cmd("plugins.list", "Plugins", "System").slash("plugins"));
        if self.builtin_active("opencode.diffs") {
            v.push(cmd("diff.open", "Open diff viewer", "VCS").slash("diff"));
        }
        if self.builtin_active("opencode.stats") {
            v.push(cmd("stats.open", "Usage statistics", "System").slash("stats"));
        }
        v
    }

    pub fn current_model_info(&self) -> Option<ModelInfo> {
        let m = self.current_model()?;
        self.model_list().into_iter().find(|x| x.provider_id == m.provider_id && x.id == m.id)
    }

    pub fn queued_prompts(&self) -> Vec<InboxItem> {
        self.session_id()
            .and_then(|s| self.store.pending.get(s))
            .map(|p| p.iter().filter(|i| i.kind == "user" && i.delivery == "queue").cloned().collect())
            .unwrap_or_default()
    }

    pub fn sidebar_visible(&self) -> bool {
        let Some(s) = self.session() else { return false };
        if s.parent_id.is_some() {
            return false;
        }
        if let Some(open) = self.views.get(&s.id).and_then(|v| v.sidebar_open) {
            return open;
        }
        let rail = if self.vertical_tabs() { self.rail_width() } else { 0 };
        self.config.session_sidebar() == "auto" && self.size.0.saturating_sub(rail) > 120
    }

    pub fn open_dialog(&mut self, d: Dialog) {
        self.dialogs.clear();
        self.dialogs.push(d);
        self.autocomplete = None;
        self.dirty = true;
    }

    pub fn copy(&mut self, text: &str, toast: &str) {
        crate::clipboard::write(text);
        self.info(toast);
    }

    /// Run a command by id. Returns false when it did not handle the request.
    pub fn run_command(&mut self, id: &str, args: Option<String>) -> bool {
        let sid = self.session_id().map(|s| s.to_string());
        match id {
            "command.palette.show" => self.open_palette(),
            "session.list" => self.open_session_list(),
            "session.new" | "session.clear" => {
                if id == "session.clear"
                    && let Some(s) = &sid
                {
                    let root = self.store.root(s);
                    self.close_tab(&root);
                }
                self.dialogs.clear();
                self.navigate(Route::Home);
            }
            "open.menu" => self.open_open_dialog(),
            "model.list" => self.open_model_dialog(None),
            "agent.list" => {
                let current = self.current_agent().map(|a| a.id);
                let opts: Vec<Opt> = self.agents().into_iter().map(|a| Opt::new(a.id.clone(), a.id.clone()).desc(a.description.unwrap_or_default())).collect();
                let mut s = Select::new("Select agent", opts);
                s.current = current;
                self.open_dialog(Dialog::select("agent", s, Size::Medium));
            }
            "agent.cycle" | "agent.cycle.reverse" => {
                let agents = self.agents();
                if agents.is_empty() {
                    return true;
                }
                let cur = self.current_agent().map(|a| a.id).unwrap_or_default();
                let i = agents.iter().position(|a| a.id == cur).unwrap_or(0) as isize;
                let d = if id == "agent.cycle" { 1 } else { -1 };
                let n = agents.len() as isize;
                let next = agents[(((i + d) % n + n) % n) as usize].id.clone();
                self.set_agent(&next);
            }
            "variant.cycle" => {
                let Some(info) = self.current_model_info() else { return true };
                let variants: Vec<String> = info.variants.iter().map(|v| v.id.clone()).collect();
                let next = cycle_variant(self.current_variant().as_deref(), &variants);
                if let Some(mut m) = self.current_model() {
                    self.models.set_variant(&format!("{}/{}", m.provider_id, m.id), Some(next.as_deref().unwrap_or("default")));
                    m.variant = next;
                    self.set_model(m, false);
                }
            }
            "variant.list" => {
                let Some(info) = self.current_model_info().filter(|m| !m.variants.is_empty()) else {
                    self.toast(Toast::new("info", "The current model does not support any variants.").title("No variants available"));
                    return true;
                };
                let mut opts = vec![Opt::new("Default", "default")];
                opts.extend(info.variants.iter().filter(|v| v.id != "default").map(|v| Opt::new(v.id.clone(), v.id.clone())));
                let mut s = Select::new("Select variant", opts);
                s.flat = true;
                s.current = Some(self.current_variant().unwrap_or_else(|| "default".into()));
                self.open_dialog(Dialog::select("variant", s, Size::Medium));
            }
            "model.cycle_recent" | "model.cycle_recent_reverse" => {
                let list = self.model_list();
                let recent: Vec<ModelRef> = self
                    .models
                    .recent
                    .iter()
                    .filter(|r| list.iter().any(|m| m.provider_id == r.provider_id && m.id == r.model_id))
                    .map(|r| ModelRef { provider_id: r.provider_id.clone(), id: r.model_id.clone(), variant: None })
                    .collect();
                self.cycle_models(recent, id.ends_with("reverse"));
            }
            "model.cycle_favorite" | "model.cycle_favorite_reverse" => {
                let fav: Vec<ModelRef> = self.models.favorite.iter().map(|r| ModelRef { provider_id: r.provider_id.clone(), id: r.model_id.clone(), variant: None }).collect();
                if fav.is_empty() {
                    self.info("Add a favorite model to use this shortcut");
                    return true;
                }
                self.cycle_models(fav, id.ends_with("reverse"));
            }
            "mcp.list" => self.open_mcp_dialog(),
            "provider.connect" => self.open_integrations(true, None, false),
            "opencode.settings" => self.open_settings(None),
            "opencode.status" => self.open_status(),
            "opencode.debug" => self.open_debug(),
            "server.pair" | "location.reload" if id == "location.reload" => {
                self.toast(Toast::new("info", "Reloading configuration…").duration(30000));
                let c = self.client.clone();
                self.call(async move { c.location_reload().await }, |app, _| {
                    app.toast(Toast::new("success", "Configuration reloaded"));
                });
            }
            "server.pair" => self.open_pair(),
            "service.restart" => {
                let Some(service) = self.opts.service.clone() else { return false };
                self.dialogs.clear();
                self.toast(Toast::new("info", "Restarting service…").duration(30000));
                // Resolves once the replacement service is healthy; the event stream
                // reattaches through the reconnect loop.
                let c = self.client.clone();
                self.call(
                    async move {
                        let endpoint = (service.restart)().await?;
                        c.set_endpoint(&endpoint);
                        Ok(())
                    },
                    |app, _| app.toast(Toast::new("success", "Service restarted")),
                );
            }
            "theme.switch" => self.open_theme_dialog(),
            "theme.switch_mode" => {
                let other = self.themes.effective_mode().other();
                if !self.themes.supports(other) {
                    return false;
                }
                self.themes.lock = Some(other);
                self.themes.set_mode(other);
                self.config.set("theme.mode", Some(json!(other.as_str())));
                self.reload_theme();
            }
            "theme.mode.lock" => {
                if self.themes.lock.is_some() {
                    self.themes.lock = None;
                    self.config.set("theme.mode", Some(json!("system")));
                } else {
                    let m = self.themes.effective_mode();
                    self.themes.lock = Some(m);
                    self.config.set("theme.mode", Some(json!(m.as_str())));
                }
            }
            "help.show" => self.open_dialog(Dialog::new("help", Body::Help)),
            "docs.open" => {
                open_url("https://opencode.ai/v2/docs");
                self.dialogs.clear();
            }
            "app.exit" => self.quit = true,
            "app.console" | "app.debug" => {}
            "terminal.suspend" => {
                #[cfg(unix)]
                {
                    self.suspend_requested = true;
                }
            }
            "terminal.title.toggle" => {
                let v = !self.config.terminal_title();
                self.config.set("terminal.title", Some(json!(v)));
            }
            "app.toggle.animations" => {
                let v = !self.config.animations();
                self.config.set("animations", Some(json!(v)));
            }
            "app.toggle.file_context" => {
                let v = !self.config.prompt_editor();
                self.config.set("prompt.editor", Some(json!(v)));
            }
            "app.toggle.diffwrap" => {
                let v = if self.config.diffs_wrap() == "none" { "word" } else { "none" };
                self.config.set("diffs.wrap", Some(json!(v)));
            }
            "app.toggle.paste_summary" => {
                let v = if self.config.prompt_paste() == "full" { "compact" } else { "full" };
                self.config.set("prompt.paste", Some(json!(v)));
            }
            "session.tab.close" => {
                if let Some(s) = sid {
                    let root = self.store.root(&s);
                    self.close_tab(&root);
                }
            }
            "session.tab.reopen" => {
                if let Some(t) = self.closed_tabs.pop() {
                    self.navigate(Route::Session(t));
                }
            }
            "session.tab.next" | "session.tab.previous" => {
                if self.tabs.is_empty() {
                    return false;
                }
                let cur = sid.map(|s| self.store.root(&s));
                let i = cur.and_then(|c| self.tabs.iter().position(|t| *t == c));
                let n = self.tabs.len() as isize;
                let next = match i {
                    Some(i) => (((i as isize) + if id.ends_with("next") { 1 } else { -1 }) % n + n) % n,
                    None => 0,
                };
                let t = self.tabs[next as usize].clone();
                self.navigate(Route::Session(t));
            }
            _ if id.starts_with("session.tab.select.") => {
                let n: usize = id.rsplit('.').next().and_then(|n| n.parse().ok()).unwrap_or(1);
                match self.tabs.get(n - 1).cloned() {
                    Some(t) => self.navigate(Route::Session(t)),
                    None => return false,
                }
            }
            _ if id.starts_with("session.quick_switch.") => {
                let n: usize = id.rsplit('.').next().and_then(|n| n.parse().ok()).unwrap_or(1);
                let slots: Vec<String> = self.pins.pinned.iter().filter(|p| self.store.sessions.get(*p).is_some_and(|s| s.parent_id.is_none())).take(9).cloned().collect();
                match slots.get(n - 1).cloned() {
                    Some(t) => self.navigate(Route::Session(t)),
                    None => return false,
                }
            }
            "session.rename" => {
                let Some(s) = sid else { return false };
                match args.as_deref().map(|a| a.trim()) {
                    Some(t) if !t.is_empty() => self.rename(&s, t),
                    Some(_) => self.rename(&s, ""),
                    None => {
                        let title = self.session().map(|x| x.display_title()).unwrap_or_default();
                        let mut d = Dialog::new("rename", Body::Prompt(PromptDialog::new("Rename session", "Session title", &title)));
                        d.data = json!(s);
                        self.open_dialog(d);
                    }
                }
            }
            "session.timeline" => self.open_timeline(),
            "session.fork" => self.open_fork(None),
            "session.compact" => {
                let Some(s) = sid else { return false };
                let mid = crate::util::message_id();
                let c = self.client.clone();
                let model = self.current_model();
                self.call(
                    async move {
                        if let Some(m) = model {
                            c.switch_model(&s, &m).await?;
                        }
                        c.compact(&s, &mid).await
                    },
                    |app, item| {
                        app.store.admit_local(item);
                        app.dirty = true;
                    },
                );
            }
            "session.undo" => self.undo(),
            "session.redo" => {
                let Some(s) = sid else { return false };
                if self.session().is_none_or(|x| x.revert.is_none()) {
                    return false;
                }
                let c = self.client.clone();
                self.call(async move { c.revert_clear(&s).await }, |_, _| {});
            }
            "session.toggle.thinking" => {
                let v = if self.config.thinking() == "show" { "hide" } else { "show" };
                self.config.set("session.thinking", Some(json!(v)));
            }
            "session.toggle.scrollbar" => {
                let v = !self.config.session_scrollbar();
                self.config.set("session.scrollbar", Some(json!(v)));
            }
            "session.toggle.exploration_grouping" => {
                let v = if self.config.grouping() == "none" { "auto" } else { "none" };
                self.config.set("session.grouping", Some(json!(v)));
            }
            "session.verbosity.cycle" => {
                let v = match self.config.verbosity() {
                    "low" => "medium",
                    "medium" => "high",
                    _ => "low",
                };
                self.config.set("session.verbosity", Some(json!(v)));
            }
            "messages.copy" => {
                let Some(s) = sid else { return false };
                self.dialogs.clear();
                match last_assistant_text(self.store.messages(&s), self.session().and_then(|x| x.revert.as_ref()).map(|r| r.message_id.as_str())) {
                    None => self.toast(Toast::new("error", "No assistant messages found")),
                    Some(t) if t.is_empty() => self.toast(Toast::new("error", "No text content found in last assistant message")),
                    Some(t) => {
                        crate::clipboard::write(&t);
                        self.toast(Toast::new("success", "Message copied to clipboard!"));
                    }
                }
            }
            "session.copy.id" => {
                let Some(s) = sid else { return false };
                self.dialogs.clear();
                crate::clipboard::write(&s);
                self.toast(Toast::new("success", "Session ID copied to clipboard!"));
            }
            "session.copy" => {
                let Some(s) = sid else { return false };
                let text = self.transcript_markdown(&s, true, true);
                crate::clipboard::write(&text);
                self.toast(Toast::new("success", "Session transcript copied to clipboard!"));
            }
            "session.export" => {
                if sid.is_none() {
                    return false;
                }
                self.open_dialog(Dialog::new("export", Body::Export(crate::ui::dialog::ExportOptions::default())));
            }
            "session.background" => {
                let Some(s) = sid else { return false };
                if !self.running() || self.autocomplete.is_some() {
                    return false;
                }
                let c = self.client.clone();
                self.call(async move { c.background(&s).await }, |_, _| {});
            }
            "session.child.first" => {
                if sid.is_none() {
                    return false;
                }
                self.toggle_composer();
            }
            "session.queued_prompts" => self.open_queued(),
            "session.parent" => {
                let Some(p) = self.session().and_then(|s| s.parent_id.clone()) else { return false };
                self.navigate(Route::Session(p));
            }
            "session.page.up" | "session.page.down" | "session.half.page.up" | "session.half.page.down" | "session.line.up" | "session.line.down" | "session.first" | "session.last" => {
                if sid.is_none() {
                    return false;
                }
                let h = self.transcript_rect.height.max(1) as usize;
                let amount = match id {
                    "session.page.up" | "session.page.down" => h / 2,
                    "session.half.page.up" | "session.half.page.down" => h / 4,
                    "session.first" | "session.last" => usize::MAX / 4,
                    _ => 1,
                };
                if id == "session.first" {
                    if let Some(s) = sid.clone() {
                        self.fetch_older(&s);
                    }
                }
                let up = id.ends_with("up") || id == "session.first";
                self.scroll(up, amount.max(1));
            }
            "session.messages_last_user" | "session.message.next" | "session.message.previous" | "session.message.user.next" | "session.message.user.previous" => {
                if sid.is_none() {
                    return false;
                }
                self.navigate_message(id);
            }
            "session.interrupt" => return self.interrupt_key(),
            "pane.focus.left" | "pane.focus.right" => {
                let panel = self.active_panel().is_some();
                if (self.terminal_visible().is_none() && !panel) || !self.dialogs.is_empty() {
                    return false;
                }
                // A full-screen panel has no session pane beside it to focus.
                if id == "pane.focus.left" && self.panel_fullscreen() {
                    return false;
                }
                self.right_focus = id == "pane.focus.right";
                if panel {
                    self.dirty = true;
                    return true;
                }
                if self.right_focus
                    && let Some(p) = self.terminal_visible()
                    && let Some(st) = self.terminal_states.get_mut(&p)
                {
                    st.interact();
                }
            }
            "terminal.toggle" => {
                let Some(s) = sid else { return false };
                if self.active_panel().is_some() && self.terminal_visible().is_some() {
                    self.close_panel();
                }
                if self.terminal_visible().is_some() && self.active_panel().is_none() {
                    self.terminal_open = false;
                    self.right_focus = false;
                } else {
                    self.terminal_open = true;
                    let (list, selected) = self.session_terminals(&s);
                    match selected.or_else(|| list.first().and_then(|t| t.get("id").and_then(|x| x.as_str()).map(|x| x.to_string()))) {
                        Some(p) => self.select_terminal(&s, Some(&p)),
                        None => self.new_terminal(&s),
                    }
                }
            }
            "terminal.close" => {
                let Some(s) = sid else { return false };
                if self.terminal_visible().is_none() {
                    return false;
                }
                self.select_terminal(&s, None);
            }
            "terminal.select" => {
                let Some(s) = sid else { return false };
                if self.panel_fullscreen() {
                    self.close_panel();
                }
                let (list, selected) = self.session_terminals(&s);
                let mut opts: Vec<Opt> = list
                    .iter()
                    .filter_map(|t| {
                        let id = t.get("id")?.as_str()?.to_string();
                        let title = t.get("foregroundProcess").and_then(|x| x.as_str()).or(t.get("title").and_then(|x| x.as_str())).unwrap_or("Terminal").to_string();
                        Some(Opt::new(title, id))
                    })
                    .collect();
                opts.push(Opt::new("+ New terminal", "new"));
                let mut sel = Select::new("Terminals", opts);
                sel.render_filter = false;
                sel.current = selected;
                self.open_dialog(Dialog::select("terminal", sel, Size::Medium));
            }
            "session.terminal" => {
                let Some(s) = sid else { return false };
                self.terminal_open = true;
                self.new_terminal(&s);
            }
            "session.sidebar.toggle" => {
                let Some(s) = sid else { return false };
                self.terminal_open = false;
                self.right_focus = false;
                let visible = self.sidebar_visible() && self.active_panel().is_none();
                if !visible && self.active_panel().is_some() {
                    self.close_panel();
                }
                self.view(&s).sidebar_open = Some(!visible);
                self.config.set("session.sidebar", Some(json!(if visible { "hide" } else { "auto" })));
            }
            "session.aside" => {
                let Some(s) = sid else {
                    self.toast(Toast::new("warning", "Open a session first"));
                    return true;
                };
                match args.filter(|a| !a.trim().is_empty()) {
                    Some(q) => self.side_question(&s, &q),
                    None => {
                        let mut d = Dialog::new("btw", Body::Prompt(PromptDialog::new("/btw", "Ask anything", "")));
                        d.data = json!(s);
                        self.open_dialog(d);
                    }
                }
            }
            "session.cd" => self.change_directory(args.unwrap_or_default()),
            "plugins.list" => self.open_plugins(),
            "diff.open" => self.open_diff(),
            "stats.open" => self.open_stats(),
            "session.move" => self.open_worktrees_dialog(),
            "prompt.images.view" => {
                self.toast(Toast::new("warning", "This view is not available in this build"));
            }
            "prompt.skills" => {
                let skills = self.loc().and_then(|l| l.skills.clone()).unwrap_or_default();
                let longest = skills.iter().map(|s| s.name.len()).max().unwrap_or(0);
                let opts: Vec<Opt> = skills
                    .iter()
                    .map(|s| Opt::new(format!("{:<longest$}", s.name), s.id.clone()).desc(locale::collapse_ws(s.description.as_deref().unwrap_or(""))))
                    .collect();
                let mut sel = Select::new("Skills", opts);
                sel.empty = Some("No skills available".into());
                sel.no_match = Some("No skills found".into());
                self.open_dialog(Dialog::select("skill", sel, Size::Large));
            }
            "prompt.stash" => {
                if self.editor.is_empty() {
                    return false;
                }
                self.stash.push(self.editor.info());
                self.editor.clear();
            }
            "prompt.stash.pop" => match self.stash.pop() {
                Some(p) => self.editor.set(&p),
                None => return false,
            },
            "prompt.stash.list" => self.open_stash(),
            "prompt.editor" => self.open_external_editor(),
            "prompt.clear" => {
                if self.editor.is_empty() {
                    return false;
                }
                let info = self.editor.info();
                if info.text.trim().chars().count() >= 20 || !info.pasted.is_empty() || !info.files.is_empty() || !info.agents.is_empty() {
                    self.history.append(info);
                }
                self.editor.clear();
                self.autocomplete = None;
            }
            "prompt.submit" => {
                self.submit("steer");
                self.dialogs.clear();
            }
            "prompt.queue" => self.submit("queue"),
            "prompt.paste" => self.paste_clipboard(),
            _ => return false,
        }
        self.dirty = true;
        true
    }

    fn cycle_models(&mut self, list: Vec<ModelRef>, reverse: bool) {
        if list.is_empty() {
            return;
        }
        let cur = self.current_model();
        let i = cur.and_then(|c| list.iter().position(|m| m.provider_id == c.provider_id && m.id == c.id));
        let n = list.len() as isize;
        let next = match i {
            Some(i) => (((i as isize) + if reverse { -1 } else { 1 }) % n + n) % n,
            None => 0,
        };
        let m = list[next as usize].clone();
        self.set_model(m, true);
    }

    pub fn rename(&mut self, sid: &str, title: &str) {
        let c = self.client.clone();
        let s = sid.to_string();
        let t = title.to_string();
        self.spawn(async move { c.session_update(&s, json!({ "title": t })).await }, |app, r| {
            if let Err(e) = r {
                app.toast(Toast::new("error", format!("Failed to rename session: {}", crate::api::error_message(&e))));
            }
        });
    }

    fn undo(&mut self) {
        let Some(sid) = self.session_id().map(|s| s.to_string()) else { return };
        let boundary = self.session().and_then(|s| s.revert.as_ref().map(|r| r.message_id.clone()));
        let target = self
            .store
            .messages(&sid)
            .iter()
            .rev()
            .filter(|m| m.kind == "user")
            .find(|m| boundary.as_ref().is_none_or(|b| m.id < *b))
            .cloned();
        let Some(target) = target else {
            self.info("Nothing to undo");
            return;
        };
        let c = self.client.clone();
        let s2 = sid.clone();
        let mid = target.id.clone();
        let running = self.running();
        self.call(
            async move {
                if running {
                    c.interrupt(&s2, false).await?;
                    c.wait(&s2).await?;
                }
                c.revert_stage(&s2, &mid).await
            },
            move |app, _| {
                let mut info = crate::state::PromptInfo::empty();
                info.text = target.text.clone().unwrap_or_default();
                app.editor.set(&info);
            },
        );
    }

    pub fn transcript_markdown(&self, sid: &str, thinking: bool, tools: bool) -> String {
        let Some(s) = self.store.session(sid) else { return String::new() };
        let mut body = Vec::new();
        for m in self.store.messages(sid) {
            match m.kind.as_str() {
                "user" => body.push(format!("## User\n\n{}", m.text_str())),
                "shell" => body.push(format!("## Shell\n\n```\n$ {}\n{}\n```", m.command.as_deref().unwrap_or(""), m.output.as_ref().map(|o| o.output.as_str()).unwrap_or(""))),
                "assistant" => {
                    let mut parts = Vec::new();
                    for c in &m.content {
                        match c {
                            Content::Text { text, .. } => parts.push(text.clone()),
                            Content::Reasoning { text, .. } if thinking => parts.push(format!("_Thinking:_\n\n{text}")),
                            Content::Tool(t) if tools => {
                                let input = match &t.state.input {
                                    Value::String(s) => s.clone(),
                                    v => serde_json::to_string_pretty(v).unwrap_or_default(),
                                };
                                let output = if t.state.status == "error" {
                                    t.state.error.as_ref().map(|e| e.message.clone()).unwrap_or_default()
                                } else {
                                    crate::ui::transcript::tool_output(t).unwrap_or_default()
                                };
                                parts.push(format!("**Tool: {}**\n\n**Input:**\n```json\n{input}\n```\n\n{output}", t.name));
                            }
                            _ => {}
                        }
                    }
                    if !parts.is_empty() {
                        body.push(format!("## Assistant\n\n{}", parts.join("\n\n")));
                    }
                }
                _ => {}
            }
        }
        let fmt = |ms: i64| {
            use chrono::{Local, TimeZone};
            Local.timestamp_millis_opt(ms).single().map(|d| d.format("%-m/%-d/%Y, %-I:%M:%S %p").to_string()).unwrap_or_default()
        };
        format!(
            "# {}\n\n**Session ID:** {}\n**Created:** {}\n**Updated:** {}\n\n---\n\n{}\n",
            s.display_title(),
            s.id,
            fmt(s.time.created),
            fmt(s.time.updated),
            body.join("\n\n---\n\n")
        )
    }

    /// Export flow (`DialogExportOptions` → copy or temp file → `DialogExportResult`).
    pub fn export_session(&mut self, opts: crate::ui::dialog::ExportOptions, copy: bool) {
        let Some(sid) = self.session_id().map(|s| s.to_string()) else { return };
        self.dialogs.clear();
        let done = move |app: &mut App, text: String, ext: &str| {
            if copy {
                app.copy(&text, "Copied to clipboard");
                return;
            }
            let path = std::env::temp_dir().join(format!("opencode-{}.{ext}", crate::util::id_body(false)));
            match std::fs::write(&path, text) {
                Ok(_) => {
                    let t = app.theme().dialog().clone();
                    let lines = vec![crate::ui::text::SLine::new(vec![crate::ui::text::seg(path.to_string_lossy().to_string(), crate::ui::text::fg(t.text.base))])];
                    let mut td = TextDialog::new("Session exported", lines);
                    td.button = Some("Close".into());
                    app.open_dialog(Dialog::text("export-result", td));
                }
                Err(e) => app.toast(Toast::new("error", e.to_string())),
            }
        };
        if opts.json {
            let c = self.client.clone();
            let sanitize = opts.sanitize;
            self.call(async move { c.export(&sid, sanitize).await }, move |app, v| {
                let text = serde_json::to_string_pretty(&v).unwrap_or_default();
                done(app, text, "json");
            });
        } else {
            let text = self.transcript_markdown(&sid, opts.thinking, opts.tools);
            done(self, text, "md");
        }
    }

    fn side_question(&mut self, sid: &str, question: &str) {
        // session.generate exposes the session's tools but runs no tool loop, so a
        // tool call would surface as an empty answer.
        const INSTRUCTIONS: &str = "The user is asking a quick side question about the conversation so far. Answer directly and concisely in markdown from what you already know. Do not call any tools and do not take any actions.";
        let c = self.client.clone();
        let s = sid.to_string();
        let q = question.to_string();
        let prompt = format!("{INSTRUCTIONS}\n\n{q}");
        self.call(async move { c.generate(&s, &prompt).await }, move |app, text| {
            let text = text.trim().to_string();
            let d = btw_answer(app, &q, &text, false);
            app.open_dialog(d);
        });
    }

    fn change_directory(&mut self, input: String) {
        let input = input.trim().to_string();
        if input.is_empty() {
            self.toast(Toast::new("error", "Directory is required"));
            return;
        }
        let base = self.location_dir();
        let dir = if let Some(rest) = input.strip_prefix("~") {
            format!("{}{}", dirs::home_dir().map(|h| h.to_string_lossy().to_string()).unwrap_or_default(), rest)
        } else if input.starts_with('/') {
            input.clone()
        } else {
            format!("{}/{}", base.trim_end_matches('/'), input)
        };
        let dir = std::path::Path::new(&dir).canonicalize().map(|p| p.to_string_lossy().to_string()).unwrap_or(dir);
        let c = self.client.clone();
        match self.session_id().map(|s| s.to_string()) {
            Some(s) => {
                let d = dir.clone();
                self.spawn(async move { c.session_move(&s, &d).await }, |app, r| {
                    if let Err(e) = r {
                        app.toast(Toast::new("error", crate::api::error_message(&e)).title("Failed to change directory"));
                    }
                });
            }
            None => {
                let d = dir.clone();
                self.spawn(async move { c.location_get(Some(&d)).await }, |app, r| match r {
                    Ok(info) => {
                        app.opts.directory = info.directory.clone();
                        app.client.directory = info.directory.clone();
                        app.store.directory = info.directory.clone();
                        let d = info.directory.clone();
                        app.store.location_mut(&d).info = Some(info);
                        app.sync_location();
                    }
                    Err(e) => app.toast(Toast::new("error", crate::api::error_message(&e)).title("Failed to change directory")),
                });
            }
        }
    }

    fn open_external_editor(&mut self) {
        self.editor_requested = true;
    }

    fn paste_clipboard(&mut self) {
        if let Some(png) = crate::clipboard::read_png() {
            let label = self.editor.next_attachment_label("Image");
            self.editor.insert_chip(&label, crate::ui::prompt::ChipKind::Paste, json!({ "uri": format!("data:image/png;base64,{png}"), "name": "clipboard", "mime": "image/png" }), true);
            return;
        }
        if let Some(text) = crate::clipboard::read_text() {
            self.paste(&text);
        }
    }

    // ---- dialogs -----------------------------------------------------------

    pub fn open_palette(&mut self) {
        let kb = &self.keybinds;
        let cmds: Vec<Cmd> = self.commands().into_iter().filter(|c| c.palette && c.id != "command.palette.show").collect();
        let mut opts: Vec<Opt> = Vec::new();
        for c in cmds.iter().filter(|c| c.suggested) {
            let mut o = Opt::new(c.title.clone(), format!("suggested:{}", c.id)).cat("Suggested");
            o.footer = Some(kb.shortcut_all(&c.id)).filter(|f| !f.is_empty());
            o.description = c.description.clone();
            opts.push(o);
        }
        for c in &cmds {
            let mut o = Opt::new(c.title.clone(), c.id.clone()).cat(c.group);
            let footer = kb.shortcut_all(&c.id);
            o.footer = Some(footer.clone()).filter(|f| !f.is_empty());
            o.description = c.description.clone();
            o.search_text = Some(format!("{} {}", c.id, c.description.clone().unwrap_or_default()));
            o.search_footer = Some(if footer.is_empty() { c.group.to_string() } else { format!("{} · {footer}", c.group) });
            opts.push(o);
        }
        for (cat, title, path, _, _, _) in settings() {
            let mut o = Opt::new(title, format!("setting:{path}")).cat("__settings");
            o.search_footer = Some(format!("Settings · {cat}"));
            o.search_text = Some(format!("{cat} {path}"));
            o.disabled = true;
            opts.push(o);
        }
        let mut s = Select::new("Commands", opts);
        s.flat = true;
        s.threshold = 0.7;
        self.open_dialog(Dialog::select("palette", s, Size::Medium));
    }

    /// `hasConnectedProvider`: some integration has a connection.
    pub fn has_connected_provider(&self) -> bool {
        self.loc()
            .and_then(|l| l.integrations.as_ref())
            .is_some_and(|list| list.iter().any(|i| i.get("connections").and_then(|c| c.as_array()).is_some_and(|a| !a.is_empty())))
    }

    /// `DialogModel` (legacy layout): Favorites and Recent sections while not searching, then every
    /// model in the server's order (grouped by provider when connected, free first).
    pub fn open_model_dialog(&mut self, provider: Option<String>) {
        let models: Vec<ModelInfo> = self.model_list().into_iter().filter(|m| m.status != "deprecated" && provider.as_ref().is_none_or(|p| &m.provider_id == p)).collect();
        let providers = self.providers();
        let pname = |id: &str| providers.iter().find(|p| p.id == id).map(|p| p.name.clone()).filter(|n| !n.is_empty()).unwrap_or_else(|| id.to_string());
        let connected = self.has_connected_provider();
        let show_extra = connected && provider.is_none();
        let key = |m: &ModelInfo| format!("{}/{}", m.provider_id, m.id);
        let mut opts: Vec<Opt> = Vec::new();
        let mut sectioned: Vec<String> = Vec::new();
        if show_extra {
            let favorites = self.models.favorite.clone();
            for f in &favorites {
                if let Some(m) = models.iter().find(|m| m.provider_id == f.provider_id && m.id == f.model_id) {
                    let mut o = Opt::new(m.name.clone(), format!("section:{}", key(m))).cat("Favorites").desc(pname(&m.provider_id));
                    if m.free() {
                        o.footer = Some("Free".into());
                    }
                    opts.push(o);
                    sectioned.push(key(m));
                }
            }
            for r in &self.models.recent {
                if favorites.iter().any(|f| f.provider_id == r.provider_id && f.model_id == r.model_id) {
                    continue;
                }
                if let Some(m) = models.iter().find(|m| m.provider_id == r.provider_id && m.id == r.model_id) {
                    let mut o = Opt::new(m.name.clone(), format!("section:{}", key(m))).cat("Recent").desc(pname(&m.provider_id));
                    if m.free() {
                        o.footer = Some("Free".into());
                    }
                    opts.push(o);
                    sectioned.push(key(m));
                }
            }
        }
        // `sortModelOptions`: a stable sort keeping the server's order within each group.
        let mut rest: Vec<&ModelInfo> = models.iter().collect();
        rest.sort_by(|a, b| {
            let grouped = if connected {
                (b.provider_id == "opencode-go").cmp(&(a.provider_id == "opencode-go")).then((b.provider_id == "opencode").cmp(&(a.provider_id == "opencode"))).then(locale::compare(&pname(&a.provider_id), &pname(&b.provider_id)))
            } else {
                std::cmp::Ordering::Equal
            };
            grouped.then(b.free().cmp(&a.free()))
        });
        for m in rest {
            let mut o = Opt::new(m.name.clone(), key(m));
            if connected {
                o.category = Some(pname(&m.provider_id));
            }
            if connected && self.models.is_favorite(&m.provider_id, &m.id) {
                o.description = Some("(Favorite)".into());
            }
            if m.free() {
                o.footer = Some("Free".into());
            }
            // Shown in a section above; the full list holds it again only while searching.
            o.disabled = sectioned.contains(&key(m));
            opts.push(o);
        }
        let title = provider.as_ref().map(|p| pname(p)).unwrap_or_else(|| "Select model".into());
        let mut s = Select::new(title, opts);
        s.flat = true;
        s.current = self.current_model().map(|m| format!("{}/{}", m.provider_id, m.id));
        s.actions = vec![
            Action::new("model.dialog.provider", if connected { "Connect an integration" } else { "View all integrations" }).dialog_level(),
            Action::new("model.dialog.favorite", "Favorite").hidden(!connected),
        ];
        let mut d = Dialog::select("model", s, Size::Medium);
        d.data = json!({ "provider": provider });
        self.open_dialog(d);
    }

    pub fn open_theme_dialog(&mut self) {
        let names = self.themes.names();
        let opts: Vec<Opt> = names.iter().map(|n| Opt::new(n.clone(), n.clone())).collect();
        let mut s = Select::new("Themes", opts);
        s.current = Some(self.themes.active.clone());
        self.theme_preview_initial = Some(self.themes.active.clone());
        self.open_dialog(Dialog::select("theme", s, Size::Medium));
    }

    pub fn open_settings(&mut self, focus: Option<&str>) {
        let mut opts = Vec::new();
        for (cat, title, path, values, labels, default) in settings() {
            let value = self.config.get(path).cloned().unwrap_or(default);
            let footer = if path == "theme.name" {
                self.themes.active.clone()
            } else if path == "scroll.speed" {
                format!("{:.2}", value.as_f64().unwrap_or(3.0))
            } else if path == "leader.timeout" {
                format!("{} ms", value.as_f64().unwrap_or(2000.0) as i64)
            } else if path == "attention.volume" {
                format!("{}%", (value.as_f64().unwrap_or(0.4) * 100.0).round() as i64)
            } else if let Some(i) = values.iter().position(|v| *v == value) {
                labels.get(i).map(|s| s.to_string()).unwrap_or_else(|| value.as_str().map(|s| s.to_string()).unwrap_or_else(|| value.to_string()))
            } else {
                value.as_str().map(|s| s.to_string()).unwrap_or_else(|| value.to_string())
            };
            opts.push(Opt::new(title, path).cat(cat).foot(footer));
        }
        let mut s = Select::new("Settings", opts);
        s.current = Some(focus.unwrap_or("theme.name").to_string());
        s.hints = vec![("change".into(), "←/→".into())];
        s.actions = vec![
            Action::key("left"),
            Action::key("right"),
        ];
        let mut d = Dialog::select("settings", s, Size::Medium);
        if let Some(sel) = d.sel() {
            sel.focus_current_option();
        }
        self.open_dialog(d);
    }

    /// Step a setting value (`left`/`right`).
    pub fn change_setting(&mut self, path: &str, dir: i64) {
        let Some((_, _, _, values, _, default)) = settings().into_iter().find(|s| s.2 == path) else { return };
        let current = self.config.get(path).cloned().unwrap_or(default);
        let next = match path {
            "theme.name" => {
                let names = self.themes.names();
                let i = names.iter().position(|n| *n == self.themes.active).unwrap_or(0) as i64;
                let n = names.len() as i64;
                let name = names[(((i + dir) % n + n) % n) as usize].clone();
                self.set_theme(&name, true);
                return;
            }
            "scroll.speed" => json!(((current.as_f64().unwrap_or(3.0) + 0.25 * dir as f64).clamp(0.25, 10.0) * 100.0).round() / 100.0),
            "leader.timeout" => json!((current.as_f64().unwrap_or(2000.0) as i64 + 250 * dir).clamp(250, 10000)),
            "attention.volume" => json!(((current.as_f64().unwrap_or(0.4) + 0.1 * dir as f64).clamp(0.0, 1.0) * 10.0).round() / 10.0),
            _ => {
                let i = values.iter().position(|v| *v == current).unwrap_or(0) as i64;
                let n = values.len() as i64;
                values[(((i + dir) % n + n) % n) as usize].clone()
            }
        };
        self.config.set(path, Some(next.clone()));
        if path == "theme.mode" {
            let mode = next.as_str().unwrap_or("system");
            self.themes.lock = crate::theme::Mode::parse(mode);
            if let Some(m) = self.themes.lock {
                self.themes.set_mode(m);
            }
            self.reload_theme();
        }
        if path == "leader.timeout" {
            self.keybinds = self.config.keybinds();
        }
        if path == "session.permissions" {
            self.autoaccept = next == json!("autoaccept");
        }
        // Rebuild the settings dialog keeping the selection.
        let selected = self.dialogs.last_mut().and_then(|d| d.sel().map(|s| s.selected));
        self.open_settings(Some(path));
        if let (Some(sel), Some(i)) = (self.dialogs.last_mut().and_then(|d| d.sel()), selected) {
            sel.selected = i;
        }
    }

    fn open_timeline(&mut self) {
        let Some(sid) = self.session_id().map(|s| s.to_string()) else { return };
        let opts: Vec<Opt> = self
            .store
            .messages(&sid)
            .iter()
            .rev()
            .filter(|m| m.kind == "user")
            .map(|m| Opt::new(m.text_str().replace('\n', " "), m.id.clone()).foot(locale::time(m.time.created)))
            .collect();
        let s = Select::new("Timeline", opts);
        self.open_dialog(Dialog::select("timeline", s, Size::Large));
    }

    pub fn open_fork(&mut self, message: Option<String>) {
        let Some(sid) = self.session_id().map(|s| s.to_string()) else { return };
        if let Some(m) = message {
            self.fork(&sid, Some(m));
            return;
        }
        let mut opts = vec![Opt::new("Full session", "full")];
        opts.extend(self.store.messages(&sid).iter().rev().filter(|m| m.kind == "user").map(|m| Opt::new(m.text_str().replace('\n', " "), m.id.clone()).foot(locale::time(m.time.created))));
        self.open_dialog(Dialog::select("fork", Select::new("Fork session", opts), Size::Large));
    }

    pub fn fork(&mut self, sid: &str, before: Option<String>) {
        // The pending view (`DialogFork`): the "Fork session" header and a spinner at the list inset.
        let spin = if self.config.animations() { format!("{} Forking session…", crate::ui::text::SPINNER_GLYPH) } else { "⋯ Forking session…".to_string() };
        let muted = self.theme().text.muted;
        let mut td = TextDialog::new("Fork session", vec![crate::ui::text::SLine::new(vec![crate::ui::text::seg(spin, crate::ui::text::fg(muted))])]);
        td.inset = Some(crate::ui::dialog::DIALOG_LIST_INSET);
        let mut d = Dialog::text("fork-pending", td);
        d.size = Size::Large;
        self.dialogs.clear();
        self.open_dialog(d);
        let c = self.client.clone();
        let s = sid.to_string();
        let prompt = before.as_ref().and_then(|b| self.store.messages(sid).iter().find(|m| &m.id == b && m.kind == "user").map(super::go_upsell::projected_prompt));
        self.spawn(async move { c.session_fork(&s, before.as_deref()).await }, move |app, r| {
            let info = match r {
                Ok(info) => info,
                Err(e) => {
                    app.toast(Toast::new("error", crate::api::error_message(&e)).duration(5000));
                    app.dialogs.clear();
                    return;
                }
            };
            let id = info.id.clone();
            app.store.remember(info);
            app.navigate(Route::Session(id));
            if let Some(p) = prompt {
                app.editor.set(&p);
            }
            app.dialogs.clear();
            app.toast(Toast::new("success", "Forked session").duration(4000));
        });
    }

    fn open_queued(&mut self) {
        let opts: Vec<Opt> = self.queued_prompts().iter().map(|i| Opt::new(i.payload.get("text").and_then(|t| t.as_str()).unwrap_or("").replace('\n', " "), i.id.clone())).collect();
        let mut s = Select::new("Queued prompts", opts);
        s.actions = vec![
            Action::new("queued_prompt.undo", "undo"),
            Action::new("queued_prompt.delete", "delete"),
        ];
        self.open_dialog(Dialog::select("queued", s, Size::Medium));
    }

    fn open_stash(&mut self) {
        let now = locale::now_ms();
        let opts: Vec<Opt> = self
            .stash
            .entries
            .iter()
            .enumerate()
            .rev()
            .map(|(i, e)| {
                let first = e.prompt.text.lines().next().unwrap_or("").trim().to_string();
                let diff = (now - e.timestamp) / 1000;
                let when = if diff < 60 {
                    "just now".to_string()
                } else if diff < 3600 {
                    format!("{}m ago", diff / 60)
                } else if diff < 86400 {
                    format!("{}h ago", diff / 3600)
                } else if diff < 86400 * 7 {
                    format!("{}d ago", diff / 86400)
                } else {
                    locale::datetime(e.timestamp)
                };
                let lines = e.prompt.text.lines().count();
                let mut o = Opt::new(locale::truncate(&first, 50), i.to_string()).desc(when);
                if lines > 1 {
                    o.footer = Some(format!("~{lines} lines"));
                }
                o
            })
            .collect();
        let mut s = Select::new("Stash", opts);
        s.actions = vec![Action::new("stash.delete", "delete")];
        self.open_dialog(Dialog::select("stash", s, Size::Medium));
    }

    fn open_mcp_dialog(&mut self) {
        let mut servers = self.loc().and_then(|l| l.mcp.clone()).unwrap_or_default();
        servers.sort_by(|a, b| a.name.cmp(&b.name));
        let t = self.theme().clone();
        let opts: Vec<Opt> = servers
            .iter()
            .map(|s| {
                let (footer, color) = match s.status.status.as_str() {
                    "pending" | "loading" => ("Connecting …", t.text.muted),
                    "connected" => ("Connected ✓", t.text.feedback.success.base),
                    "failed" => ("Failed !", t.text.feedback.error.base),
                    "needs_auth" => ("Sign in required →", t.text.feedback.warning.base),
                    _ => ("Disabled ○", t.text.muted),
                };
                let mut o = Opt::new(s.name.clone(), s.name.clone()).foot(footer);
                o.footer_color = Some(color);
                o
            })
            .collect();
        let mut sel = Select::new("MCP servers", opts);
        sel.actions = vec![Action::new("dialog.mcp.toggle", "toggle")];
        self.open_dialog(Dialog::select("mcp", sel, Size::Medium));
    }

    fn open_status(&mut self) {
        let t = self.theme().clone();
        let servers = self.loc().and_then(|l| l.mcp.clone()).unwrap_or_default();
        let mut lines = Vec::new();
        use crate::ui::text::{SLine, Seg, bold, fg, seg};
        if servers.is_empty() {
            lines.push(SLine::new(vec![seg("No MCP servers", fg(t.text.base))]));
        } else {
            lines.push(SLine::new(vec![seg(format!("{} MCP server{}", servers.len(), if servers.len() == 1 { "" } else { "s" }), fg(t.text.base))]));
            for s in &servers {
                let (c, text) = match s.status.status.as_str() {
                    "connected" => (t.text.feedback.success.base, "Connected".to_string()),
                    "failed" => (t.text.feedback.error.base, s.status.error.clone().unwrap_or_default()),
                    "disabled" => (t.text.muted, "Disabled in configuration".to_string()),
                    "needs_auth" => (t.text.feedback.warning.base, format!("Needs authentication: {}", s.status.error.clone().unwrap_or_default())),
                    other => (t.text.muted, other.to_string()),
                };
                lines.push(SLine::new(vec![seg("• ", fg(c)), Seg::new(s.name.clone(), bold(fg(t.text.base))), seg(format!(" {text}"), fg(t.text.muted))]));
            }
        }
        self.open_dialog(Dialog::text("text", TextDialog::new("Status", lines)));
    }

    /// `DialogDebug` rows: (label, value).
    pub fn debug_entries(&self) -> Vec<(&'static str, String)> {
        let model = self.current_model().map(|m| format!("{}/{}", m.provider_id, m.id));
        vec![
            ("Version", format!("{} ({})", self.opts.version, self.opts.channel)),
            ("Date", locale::iso(locale::now_ms())),
            ("OS", crate::util::system::describe_os()),
            ("Terminal", crate::util::system::describe_terminal()),
            ("Session ID", self.session_id().map(|s| s.to_string()).unwrap_or_else(|| "n/a".into())),
            ("Model", model.unwrap_or_else(|| "n/a".into())),
        ]
    }

    pub fn open_debug(&mut self) {
        let entries = self.debug_entries();
        let text: String = entries.iter().map(|(k, v)| format!("{k}: {v}")).collect::<Vec<_>>().join("\n");
        let mut d = Dialog::text("debug", debug_dialog(self, &entries, false));
        d.size = Size::Large;
        d.data = json!({ "text": text, "entries": entries.iter().map(|(k, v)| json!([k, v])).collect::<Vec<_>>() });
        self.open_dialog(d);
    }
}

/// `DialogDebug`: label column (10 + gap), word-wrapped values, the note and `copy enter` footer.
pub fn debug_dialog(app: &App, entries: &[(&str, String)], copied: bool) -> TextDialog {
    use crate::ui::text::{SLine, Wrap, fg, seg, wrap};
    let t = app.theme().dialog().clone();
    let w = (Size::Large.width() as usize).min(app.size.0.saturating_sub(2) as usize).saturating_sub(DIALOG_INSET * 2);
    let mut lines = Vec::new();
    for (k, v) in entries {
        let rows = wrap(&[seg(v.clone(), fg(t.text.base))], w.saturating_sub(11).max(1), Wrap::Word);
        for (i, r) in rows.into_iter().enumerate() {
            let label = if i == 0 { format!("{k:<10} ") } else { " ".repeat(11) };
            lines.push(SLine::new(r).prefixed(&[seg(label, fg(t.text.muted))]));
        }
    }
    let copy = if copied { FooterItem::new("✓ copied", "enter").color(t.text.feedback.success.base) } else { FooterItem::new("copy", "enter") };
    TextDialog::new("Debug", lines).footer(Footer { children: vec![seg("Share this when reporting an issue.", fg(t.text.muted))], secondary: vec![], primary: vec![copy] })
}

/// The `/btw` answer (`Answer` in `feature-plugins/prompt/btw.tsx`).
pub fn btw_answer(app: &App, question: &str, answer: &str, copied: bool) -> Dialog {
    let base = app.theme().clone();
    let t = base.dialog().clone();
    let width = (Size::Large.width() as usize).min(app.size.0.saturating_sub(2) as usize).saturating_sub(DIALOG_INSET * 2).max(1);
    let st = crate::theme::SyntaxStyle::from_theme(&base);
    let o = crate::ui::markdown::MdOpts { theme: &base, syntax: &st, conceal: true, fg: base.markdown.text, mono: false };
    let lines = crate::ui::markdown::render(answer, width, &o);
    let question_line = question.split_whitespace().collect::<Vec<_>>().join(" ");
    let mut td = TextDialog::new("/btw", lines);
    td.title_extra = Some((question_line, true));
    td.header_gap = 2;
    td.band = Some(base.background.raised.high);
    // The scrollbox holds at most max(8, 60% of the terminal) rows, including its padding rows.
    let max = ((app.size.1 as f64 * 0.6).floor() as usize).max(8).saturating_sub(2);
    td.height = Some(max).filter(|m| td.lines.len() > *m);
    let copy = if copied { FooterItem::new("✓ copied", "").color(t.text.feedback.success.base) } else { FooterItem::new("copy", "c") };
    td.footer = Some(Footer { secondary: vec![copy, FooterItem::new("scroll", "j/k ↑/↓")], ..Default::default() });
    let mut d = Dialog::text("btw-answer", td);
    d.size = Size::Large;
    d.centered = true;
    d.data = json!({ "question": question, "answer": answer });
    d
}

/// `/copy`: the text of the latest assistant message or completed compaction with a summary
/// (whichever is later) before the revert point. `None` when there is none; empty text when the
/// assistant message has no text parts.
pub fn last_assistant_text(messages: &[Message], revert: Option<&str>) -> Option<String> {
    let m = messages
        .iter()
        .filter(|m| revert.is_none_or(|b| m.id.as_str() < b))
        .rev()
        .find(|m| m.kind == "assistant" || (m.kind == "compaction" && m.status.as_deref() == Some("completed") && m.summary.as_deref().is_some_and(|s| !s.trim().is_empty())))?;
    if m.kind == "compaction" {
        return Some(m.summary.as_deref().unwrap_or("").trim().to_string());
    }
    Some(
        m.content
            .iter()
            .filter_map(|c| if let Content::Text { text, .. } = c { Some(text.as_str()) } else { None })
            .collect::<Vec<_>>()
            .join("\n")
            .trim()
            .to_string(),
    )
}

pub fn open_url(url: &str) {
    let cmd = if cfg!(target_os = "macos") { "open" } else if cfg!(windows) { "explorer" } else { "xdg-open" };
    let _ = std::process::Command::new(cmd).arg(url).stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null()).spawn();
}
