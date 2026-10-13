//! Application state, event loop and data synchronization (`src/app.tsx`).

pub mod busy;
pub mod commands;
pub mod composer;
pub mod diffview;
pub mod stats;
pub mod input;
pub mod migration;
pub mod mini;
pub mod selection;
pub mod tabs;
pub mod terminal;
pub mod view;
pub mod workspace;

use crate::api::sse::{ConnStatus, SseMsg};
use crate::api::types::*;
use crate::api::{Client, error_message};
use crate::config::Config;
use crate::keymap::{Keybinds, Stroke};
use crate::state::{History, KeyedStore, ModelPrefs, SessionPins, Stash};
use crate::store::{Catalog, Effect, Store};
use crate::theme::{Mode, Rgba, SyntaxStyle, ThemeManager};
use crate::ui::dialog::Dialog;
use crate::ui::prompt::Editor;
use crate::ui::text::Click;
use crate::util::locale::now_ms;
use ratatui::layout::Rect;
use serde_json::Value;
use std::collections::{HashMap, HashSet};
use std::time::{Duration, Instant};
use tokio::sync::mpsc::{UnboundedSender, unbounded_channel};

/// Options for [`crate::run`].
#[derive(Clone, Debug, Default)]
pub struct TuiOptions {
    /// Server base URL, e.g. `http://127.0.0.1:4096`.
    pub url: String,
    /// Basic-auth password (username `opencode`).
    pub password: Option<String>,
    /// Launch directory (default location).
    pub directory: String,
    /// `--session <id>`.
    pub session: Option<String>,
    /// `--continue`.
    pub continue_last: bool,
    /// `--fork` (with `--session`/`--continue`).
    pub fork: bool,
    /// `--prompt <text>`.
    pub prompt: Option<String>,
    /// `--model provider/model`.
    pub model: Option<String>,
    /// `--agent <id>`.
    pub agent: Option<String>,
    /// `--auto`: start with permissions auto-accepted.
    pub auto: bool,
    /// Storage channel segment for `<state>/<channel>/tui`.
    pub channel: String,
    /// App version shown in the home footer.
    pub version: String,
    /// Terminal environment variables forwarded to new sessions.
    pub environment: HashMap<String, String>,
}

pub type Apply = Box<dyn FnOnce(&mut App) + Send>;

pub enum AppMsg {
    Sse(SseMsg),
    Apply(Apply),
}

#[derive(Clone, Debug, PartialEq)]
pub enum Route {
    Home,
    Session(String),
    /// Plugin route (`diff`, `stats`).
    Plugin(String),
}

#[derive(Clone, Debug)]
pub struct Toast {
    pub title: Option<String>,
    pub message: String,
    pub variant: String,
    pub duration: u64,
    pub action: Option<(String, String)>,
    pub shown: Option<Instant>,
}

impl Toast {
    pub fn new(variant: &str, message: impl Into<String>) -> Toast {
        Toast { title: None, message: message.into(), variant: variant.into(), duration: 5000, action: None, shown: None }
    }
    pub fn title(mut self, t: impl Into<String>) -> Toast {
        self.title = Some(t.into());
        self
    }
    pub fn duration(mut self, d: u64) -> Toast {
        self.duration = d;
        self
    }
}

/// Per-session view state.
#[derive(Clone, Debug, Default)]
pub struct SessionView {
    /// Lines scrolled up from the bottom (0 = following).
    pub scroll_from_bottom: usize,
    pub expanded: HashMap<String, bool>,
    pub sidebar_open: Option<bool>,
    pub loaded: bool,
    /// Invisible lines appended so a jump target can reach the viewport top.
    pub nav_slack: usize,
}

/// Autocomplete popup state.
#[derive(Clone, Debug, Default)]
pub struct Autocomplete {
    pub mode: String,
    pub index: usize,
    pub selected: usize,
    pub items: Vec<AcItem>,
    pub loading: bool,
    pub error: bool,
    pub query: String,
    pub armed: Option<usize>,
}

#[derive(Clone, Debug, Default)]
pub struct AcItem {
    pub display: String,
    pub description: Option<String>,
    pub label: Option<String>,
    pub value: String,
    pub kind: String,
    pub directory: bool,
    pub data: Value,
    pub destructive: Option<String>,
}

/// Permission prompt UI state.
#[derive(Clone, Debug, Default)]
pub struct PermissionUi {
    pub id: String,
    pub selected: usize,
    pub expanded: bool,
    pub rejecting: bool,
    pub reject_text: String,
}

/// Form prompt UI state.
#[derive(Clone, Debug, Default)]
pub struct FormUi {
    pub id: String,
    pub tab: usize,
    pub row: usize,
    pub answers: serde_json::Map<String, Value>,
    pub editing: bool,
    pub text: String,
    pub error: Option<String>,
    pub acknowledged: HashSet<String>,
    pub review_scroll: usize,
}

pub struct App {
    pub opts: TuiOptions,
    pub client: Client,
    pub store: Store,
    pub config: Config,
    pub keybinds: Keybinds,
    pub themes: ThemeManager,
    pub syntax: SyntaxStyle,
    pub thinking_syntax: SyntaxStyle,
    pub route: Route,
    pub editor: Editor,
    pub dialogs: Vec<Dialog>,
    pub toast: Option<Toast>,
    pub toast_queue: Vec<Toast>,
    pub views: HashMap<String, SessionView>,
    pub connection: ConnStatus,
    pub connection_since: Instant,
    pub ever_connected: bool,
    pub pending_keys: Vec<Stroke>,
    pub pending_deadline: Option<Instant>,
    pub hits: Vec<(Rect, Click)>,
    pub dialog_hits: Vec<(Rect, crate::ui::dialog::DialogHit)>,
    pub size: (u16, u16),
    pub tx: UnboundedSender<AppMsg>,
    pub quit: bool,
    pub start: Instant,
    pub history: History,
    pub stash: Stash,
    pub models: ModelPrefs,
    pub pins: SessionPins,
    pub keyed: KeyedStore,
    pub agent_current: Option<String>,
    pub agent_drafts: HashMap<String, String>,
    pub model_selection: HashMap<String, ModelRef>,
    pub autoaccept: bool,
    pub autocomplete: Option<Autocomplete>,
    pub dismissed_text: Option<String>,
    pub interrupt: u8,
    pub interrupt_at: Option<Instant>,
    pub placeholder: usize,
    pub permission_ui: Option<PermissionUi>,
    pub form_ui: Option<FormUi>,
    pub tabs: Vec<String>,
    pub closed_tabs: Vec<String>,
    pub shell_output: HashMap<String, String>,
    pub drafts: HashMap<String, crate::state::PromptInfo>,
    pub epilogue: Option<String>,
    pub default_model: Option<ModelRef>,
    pub prompt_sent: bool,
    pub theme_preview_initial: Option<String>,
    pub notified: HashSet<String>,
    pub focused: Option<bool>,
    pub prompt_rect: Rect,
    pub transcript_rect: Rect,
    pub transcript_lines: usize,
    pub frame: u64,
    pub dirty: bool,
    pub suspend_requested: bool,
    pub editor_requested: bool,
    pub dialog_rect: Option<Rect>,
    pub render_rev: u64,
    pub tab_menu: Option<tabs::TabMenu>,
    pub rail_pref: u16,
    pub rail_scroll: u16,
    pub resizing: Option<tabs::Resizing>,
    pub last_handle_click: Option<Instant>,
    pub tab_hits: Vec<(Rect, Option<String>)>,
    pub previous_route: Option<Route>,
    pub diff: Option<diffview::DiffViewer>,
    pub diff_memory: Option<String>,
    pub stats: Option<stats::StatsState>,
    pub terminal_lists: HashMap<String, Vec<Value>>,
    pub terminal_states: HashMap<String, terminal::TermState>,
    pub terminal_open: bool,
    pub terminals_available: bool,
    pub right_focus: bool,
    pub pane_pref: Option<u16>,
    pub terminal_rect: Rect,
    pub composer: Option<composer::Composer>,
    pub transcript_marks: Vec<(usize, String, bool)>,
    pub subagents_active: bool,
    pub transcript_cache: Option<(String, Vec<crate::ui::text::SLine>)>,
    /// Native-style mouse text selection over the rendered screen.
    pub selection: Option<selection::Selection>,
    /// Last rendered frame (selection text source).
    pub last_frame: Option<ratatui::buffer::Buffer>,
    /// Running as `opencode mini` (scrollback transcript + live footer).
    pub mini: bool,
    /// V1 history import badge: current progress and when the badge appeared (spinner phase).
    pub migration: Option<(migration::MigrationProgress, Instant)>,
    /// Busy turns per session (status line above the prompt; an ocd addition).
    pub busy: HashMap<String, busy::Turn>,
}

impl App {
    pub fn new(opts: TuiOptions, config: Config, tx: UnboundedSender<AppMsg>) -> App {
        let client = Client::new(&opts.url, opts.password.as_deref(), &opts.directory);
        let mode = match config.theme_mode() {
            "light" => Mode::Light,
            _ => Mode::Dark,
        };
        let lock = Mode::parse(config.theme_mode());
        let themes = ThemeManager::new(config.theme_name(), mode, lock);
        let syntax = SyntaxStyle::from_theme(&themes.current);
        let thinking_syntax = SyntaxStyle::muted(&themes.current);
        let state = crate::util::paths::state_dir();
        let keybinds = config.keybinds();
        let channel = if opts.channel.is_empty() { "latest".to_string() } else { opts.channel.clone() };
        let autoaccept = opts.auto || config.permissions_mode() == "autoaccept";
        let route = match &opts.session {
            Some(id) if !opts.fork => Route::Session(id.clone()),
            _ => Route::Home,
        };
        let mut app = App {
            client,
            store: Store::new(&opts.directory),
            keybinds,
            themes,
            syntax,
            thinking_syntax,
            route,
            editor: Editor::new(),
            dialogs: Vec::new(),
            toast: None,
            toast_queue: Vec::new(),
            views: HashMap::new(),
            connection: ConnStatus::Connecting,
            connection_since: Instant::now(),
            ever_connected: false,
            pending_keys: Vec::new(),
            pending_deadline: None,
            hits: Vec::new(),
            dialog_hits: Vec::new(),
            size: (80, 24),
            tx,
            quit: false,
            start: Instant::now(),
            history: History::load(state.join("prompt-history.jsonl")),
            stash: Stash::load(state.join("prompt-stash.jsonl")),
            models: ModelPrefs::load(state.join("model.json")),
            pins: SessionPins::load(state.join("session.json")),
            keyed: KeyedStore::new(&state, &channel),
            agent_current: opts.agent.clone(),
            agent_drafts: HashMap::new(),
            model_selection: HashMap::new(),
            autoaccept,
            autocomplete: None,
            dismissed_text: None,
            interrupt: 0,
            interrupt_at: None,
            placeholder: rand::random_range(0..3),
            permission_ui: None,
            form_ui: None,
            tabs: Vec::new(),
            closed_tabs: Vec::new(),
            shell_output: HashMap::new(),
            drafts: HashMap::new(),
            epilogue: None,
            default_model: None,
            prompt_sent: false,
            theme_preview_initial: None,
            notified: HashSet::new(),
            focused: None,
            prompt_rect: Rect::default(),
            transcript_rect: Rect::default(),
            transcript_lines: 0,
            frame: 0,
            dirty: true,
            suspend_requested: false,
            editor_requested: false,
            dialog_rect: None,
            render_rev: 0,
            tab_menu: None,
            rail_pref: tabs::SESSION_SIDEBAR_WIDTH,
            rail_scroll: 0,
            resizing: None,
            last_handle_click: None,
            tab_hits: Vec::new(),
            previous_route: None,
            diff: None,
            diff_memory: None,
            stats: None,
            terminal_lists: HashMap::new(),
            terminal_states: HashMap::new(),
            terminal_open: true,
            terminals_available: false,
            right_focus: false,
            pane_pref: None,
            terminal_rect: Rect::default(),
            composer: None,
            transcript_marks: Vec::new(),
            subagents_active: true,
            transcript_cache: None,
            selection: None,
            last_frame: None,
            mini: false,
            migration: None,
            busy: HashMap::new(),
            config,
            opts,
        };
        if let Some(p) = app.opts.prompt.clone() {
            app.editor.insert(&p);
        }
        app.load_tabs();
        let layout = app.keyed.get("layout");
        if let Some(w) = layout.as_ref().and_then(|v| v.get("verticalTabsWidth").and_then(|w| w.as_u64())) {
            app.rail_pref = w as u16;
        }
        app.pane_pref = layout.as_ref().and_then(|v| v.get("paneWidth").or_else(|| v.get("terminalWidth")).and_then(|w| w.as_u64())).map(|w| w as u16);
        app
    }

    /// Test/headless constructor.
    pub fn headless(opts: TuiOptions, config: Config) -> App {
        let (tx, _rx) = unbounded_channel();
        App::new(opts, config, tx)
    }

    // ---- async plumbing ----------------------------------------------------

    pub fn spawn<T: Send + 'static>(
        &self,
        fut: impl std::future::Future<Output = anyhow::Result<T>> + Send + 'static,
        then: impl FnOnce(&mut App, anyhow::Result<T>) + Send + 'static,
    ) {
        let Ok(handle) = tokio::runtime::Handle::try_current() else { return };
        let tx = self.tx.clone();
        handle.spawn(async move {
            let r = fut.await;
            let _ = tx.send(AppMsg::Apply(Box::new(move |app| then(app, r))));
        });
    }

    /// Spawn a request whose errors become error toasts.
    pub fn call<T: Send + 'static>(
        &self,
        fut: impl std::future::Future<Output = anyhow::Result<T>> + Send + 'static,
        ok: impl FnOnce(&mut App, T) + Send + 'static,
    ) {
        self.spawn(fut, move |app, r| match r {
            Ok(v) => ok(app, v),
            Err(e) => app.toast_error(&e),
        });
    }

    pub fn toast(&mut self, t: Toast) {
        if self.toast.is_some() && !self.toast_queue.is_empty() {
            self.toast_queue.push(t);
            return;
        }
        let mut t = t;
        t.shown = Some(Instant::now());
        self.toast = Some(t);
        self.dirty = true;
    }

    pub fn toast_error(&mut self, e: &anyhow::Error) {
        self.toast(Toast::new("error", error_message(e)));
    }

    pub fn info(&mut self, msg: &str) {
        self.toast(Toast::new("info", msg));
    }

    // ---- derived state -----------------------------------------------------

    pub fn theme(&self) -> &crate::theme::Theme {
        &self.themes.current
    }

    pub fn session_id(&self) -> Option<&str> {
        match &self.route {
            Route::Session(id) => Some(id),
            _ => None,
        }
    }

    pub fn session(&self) -> Option<&SessionInfo> {
        self.session_id().and_then(|id| self.store.session(id))
    }

    /// Directory of the current location (session location or launch directory).
    pub fn location_dir(&self) -> String {
        self.session().map(|s| s.location.directory.clone()).filter(|d| !d.is_empty()).unwrap_or_else(|| self.opts.directory.clone())
    }

    pub fn loc(&self) -> Option<&crate::store::LocationData> {
        let d = self.location_dir();
        self.store.locations.get(&d).or_else(|| self.store.locations.get(&self.opts.directory))
    }

    pub fn all_agents(&self) -> Vec<AgentInfo> {
        self.loc().and_then(|l| l.agents.clone()).unwrap_or_default()
    }

    /// Selectable agents: `mode !== "subagent" && !hidden`.
    pub fn agents(&self) -> Vec<AgentInfo> {
        self.all_agents().into_iter().filter(|a| a.mode != "subagent" && !a.hidden).collect()
    }

    pub fn current_agent(&self) -> Option<AgentInfo> {
        let agents = self.agents();
        let selected = match self.session_id() {
            Some(sid) => self
                .agent_drafts
                .get(sid)
                .cloned()
                .or_else(|| self.opts.agent.clone())
                .or_else(|| self.store.session(sid).and_then(|s| s.agent.clone()))
                .or_else(|| self.agent_current.clone()),
            None => self.agent_current.clone(),
        };
        agents.iter().find(|a| Some(&a.id) == selected.as_ref()).cloned().or_else(|| agents.first().cloned())
    }

    pub fn agent_colors(&self) -> HashMap<String, Rgba> {
        let t = self.theme();
        let mut colors: Vec<Rgba> = Vec::new();
        for i in 0..t.categorical.len() {
            let c = t.categorical(i, 200);
            if !colors.iter().any(|x| x.same_value(&c)) {
                colors.push(c);
            }
        }
        if colors.is_empty() {
            colors.push(t.text.base);
        }
        let visible: Vec<AgentInfo> = self.all_agents().into_iter().filter(|a| !a.hidden).collect();
        visible
            .iter()
            .enumerate()
            .map(|(i, a)| {
                let c = a.color.as_deref().and_then(Rgba::from_hex).unwrap_or(colors[i % colors.len()]);
                (a.id.clone(), c)
            })
            .collect()
    }

    pub fn default_agent_color(&self) -> Rgba {
        let t = self.theme();
        if t.categorical.is_empty() { t.text.base } else { t.categorical(0, 200) }
    }

    pub fn agent_color(&self, id: &str) -> Rgba {
        self.agent_colors().get(id).copied().unwrap_or_else(|| self.default_agent_color())
    }

    pub fn model_list(&self) -> Vec<ModelInfo> {
        self.loc().and_then(|l| l.models.clone()).unwrap_or_default()
    }

    pub fn providers(&self) -> Vec<ProviderInfo> {
        self.loc().and_then(|l| l.providers.clone()).unwrap_or_default()
    }

    fn selection_key(&self) -> String {
        let agent = self.current_agent().map(|a| a.id).unwrap_or_default();
        match self.session_id() {
            Some(s) => format!("{s}:{agent}"),
            None => format!("{}:{agent}", self.location_dir()),
        }
    }

    pub fn model_valid(&self, m: &ModelRef) -> bool {
        let list = self.model_list();
        list.is_empty() || list.iter().any(|x| x.provider_id == m.provider_id && x.id == m.id)
    }

    /// Current model: selection → CLI → session → agent → recent → server default.
    pub fn current_model(&self) -> Option<ModelRef> {
        if let Some(m) = self.model_selection.get(&self.selection_key()) {
            return Some(m.clone());
        }
        if let Some(cli) = &self.opts.model
            && let Some((p, m)) = cli.split_once('/')
        {
            return Some(ModelRef { provider_id: p.into(), id: m.into(), variant: None });
        }
        if let Some(m) = self.session().and_then(|s| s.model.clone()) {
            return Some(m);
        }
        if let Some(m) = self.current_agent().and_then(|a| a.model) {
            return Some(m);
        }
        let list = self.model_list();
        for r in &self.models.recent {
            if list.iter().any(|x| x.provider_id == r.provider_id && x.id == r.model_id) {
                return Some(ModelRef { provider_id: r.provider_id.clone(), id: r.model_id.clone(), variant: None });
            }
        }
        self.default_model.clone()
    }

    pub fn current_variant(&self) -> Option<String> {
        let m = self.current_model()?;
        if let Some(v) = m.variant.clone().filter(|v| v != "default") {
            return Some(v);
        }
        self.models.variant(&format!("{}/{}", m.provider_id, m.id)).filter(|v| v != "default")
    }

    pub fn set_model(&mut self, m: ModelRef, recent: bool) {
        if recent {
            self.models.add_recent(&m.provider_id, &m.id);
        }
        let key = self.selection_key();
        self.model_selection.insert(key, m);
        self.dirty = true;
    }

    pub fn set_agent(&mut self, id: &str) {
        if !self.agents().iter().any(|a| a.id == id) {
            self.toast(Toast::new("warning", format!("Agent not found: {id}")).duration(3000));
            return;
        }
        let model = self.current_model();
        self.agent_current = Some(id.to_string());
        if let Some(sid) = self.session_id().map(|s| s.to_string()) {
            self.agent_drafts.insert(sid, id.to_string());
        }
        if let Some(m) = model
            && self.session_id().is_some()
        {
            let key = self.selection_key();
            self.model_selection.entry(key).or_insert(m);
        }
        self.dirty = true;
    }

    pub fn running(&self) -> bool {
        self.session_id().is_some_and(|s| self.store.is_running(s))
    }

    pub fn connected(&self) -> bool {
        self.connection == ConnStatus::Connected
    }

    pub fn view(&mut self, sid: &str) -> &mut SessionView {
        self.views.entry(sid.to_string()).or_default()
    }

    // ---- navigation --------------------------------------------------------

    pub fn navigate(&mut self, route: Route) {
        if self.route == route {
            return;
        }
        // Save draft of the current prompt.
        let key = self.session_id().unwrap_or("").to_string();
        if !self.editor.is_empty() {
            self.drafts.insert(key, self.editor.info());
        }
        self.editor = Editor::new();
        self.autocomplete = None;
        self.permission_ui = None;
        self.form_ui = None;
        self.interrupt = 0;
        self.route = route;
        let key = self.session_id().unwrap_or("").to_string();
        if let Some(d) = self.drafts.remove(&key) {
            self.editor.set(&d);
        }
        self.placeholder = rand::random_range(0..3);
        self.right_focus = false;
        self.composer = None;
        if let Route::Session(id) = self.route.clone() {
            if self.terminals_available {
                self.refresh_terminals(&id);
            }
            self.open_tab(&id);
            self.sync_session(&id);
            self.epilogue_update();
        } else {
            self.epilogue = None;
        }
        self.dirty = true;
    }

    pub fn epilogue_update(&mut self) {
        if let Some(s) = self.session() {
            let title = crate::util::locale::truncate(&s.display_title(), 50);
            self.epilogue = Some(crate::ui::view_helpers::session_epilogue(&title, &s.id));
        }
    }

    // ---- tabs --------------------------------------------------------------

    fn tabs_key(&self) -> String {
        "tabs".into()
    }

    pub fn load_tabs(&mut self) {
        let v = self.keyed.get(&self.tabs_key()).unwrap_or(Value::Null);
        let state = if self.config.tabs_scope() == "global" { v.get("global").cloned() } else { v.get("cwd").and_then(|c| c.get(&self.opts.directory)).cloned() };
        self.tabs = state
            .and_then(|s| s.get("tabs").cloned())
            .and_then(|t| t.as_array().cloned())
            .map(|a| a.iter().filter_map(|t| t.get("sessionID").and_then(|s| s.as_str()).map(|s| s.to_string())).collect())
            .unwrap_or_default();
    }

    pub fn save_tabs(&self) {
        let tabs: Vec<Value> = self.tabs.iter().map(|id| serde_json::json!({ "sessionID": id })).collect();
        let global = self.config.tabs_scope() == "global";
        let dir = self.opts.directory.clone();
        self.keyed.update(&self.tabs_key(), move |v| {
            if !v.is_object() {
                *v = serde_json::json!({});
            }
            let state = serde_json::json!({ "tabs": tabs });
            if global {
                v["global"] = state;
            } else {
                if !v.get("cwd").is_some_and(|c| c.is_object()) {
                    v["cwd"] = serde_json::json!({});
                }
                v["cwd"][dir] = state;
            }
        });
    }

    pub fn open_tab(&mut self, id: &str) {
        if !self.config.tabs_enabled() {
            return;
        }
        let root = self.store.root(id);
        if !self.tabs.contains(&root) {
            self.tabs.push(root);
            self.save_tabs();
        }
    }

    pub fn close_tab(&mut self, id: &str) {
        let Some(pos) = self.tabs.iter().position(|t| t == id) else { return };
        let removed = self.tabs.remove(pos);
        self.closed_tabs.push(removed);
        self.save_tabs();
        let current_root = self.session_id().map(|s| self.store.root(s));
        if current_root.as_deref() == Some(id) {
            let next = self.tabs.get(pos).or_else(|| pos.checked_sub(1).and_then(|p| self.tabs.get(p))).cloned();
            match next {
                Some(n) => self.navigate(Route::Session(n)),
                None => self.navigate(Route::Home),
            }
        }
    }

    // ---- data sync ---------------------------------------------------------

    pub fn run_effects(&mut self, fx: Vec<Effect>) {
        for e in fx {
            match e {
                Effect::SessionSync(id) => self.fetch_session(&id),
                Effect::MessagesSync(id) => self.fetch_messages(&id),
                Effect::PendingSync(id) => self.fetch_pending(&id),
                Effect::ActiveSync => {
                    let c = self.client.clone();
                    self.spawn(async move { c.session_active().await }, |app, r| {
                        if let Ok(v) = r {
                            app.store.apply_active(&v);
                            app.dirty = true;
                        }
                    });
                }
                Effect::LocationSync => self.sync_location(),
                Effect::Catalog(dir, cat) => self.fetch_catalog(&dir, cat),
                Effect::MessageGet(sid, mid) => {
                    let c = self.client.clone();
                    self.spawn(async move { c.message_get(&sid, &mid).await.map(|v| (sid, v)) }, |app, r| {
                        if let Ok((sid, v)) = r
                            && let Some(m) = Message::from_value(v)
                        {
                            let list = app.store.messages.entry(sid).or_default();
                            match list.iter().position(|x| x.id == m.id) {
                                Some(i) => list[i] = m,
                                None => list.push(m),
                            }
                            app.dirty = true;
                        }
                    });
                }
            }
        }
    }

    pub fn fetch_session(&mut self, id: &str) {
        let c = self.client.clone();
        let id = id.to_string();
        self.spawn(async move { c.session_get(&id).await }, |app, r| {
            if let Ok(info) = r {
                app.store.outbox.remove(&info.id);
                app.store.remember(info);
                if app.session_id().is_some() {
                    app.epilogue_update();
                }
                app.dirty = true;
            }
        });
    }

    pub fn fetch_messages(&mut self, id: &str) {
        let c = self.client.clone();
        let sid = id.to_string();
        self.spawn(async move { c.messages(&sid, 100, None).await.map(|p| (sid, p)) }, |app, r| {
            if let Ok((sid, page)) = r {
                let msgs: Vec<Message> = page.data.into_iter().filter_map(Message::from_value).collect();
                app.store.apply_messages(&sid, msgs, page.cursor.next);
                app.dirty = true;
            }
        });
    }

    pub fn fetch_older(&mut self, id: &str) {
        let Some(Some(cursor)) = self.store.message_cursor.get(id).cloned() else { return };
        let c = self.client.clone();
        let sid = id.to_string();
        self.store.message_cursor.insert(sid.clone(), None);
        self.spawn(async move { c.messages(&sid, 100, Some(&cursor)).await.map(|p| (sid, p)) }, |app, r| {
            if let Ok((sid, page)) = r {
                let msgs: Vec<Message> = page.data.into_iter().filter_map(Message::from_value).collect();
                app.store.prepend_messages(&sid, msgs, page.cursor.next);
                app.dirty = true;
            }
        });
    }

    pub fn fetch_pending(&mut self, id: &str) {
        let c = self.client.clone();
        let sid = id.to_string();
        self.spawn(async move { c.inbox(&sid).await.map(|p| (sid, p)) }, |app, r| {
            if let Ok((sid, items)) = r {
                app.store.apply_pending(&sid, items);
                app.dirty = true;
            }
        });
    }

    /// Load everything a session view needs.
    pub fn sync_session(&mut self, id: &str) {
        if id == "dummy" {
            return;
        }
        if !self.store.loaded.contains(&format!("session:{id}")) {
            self.fetch_session(id);
        }
        if !self.store.loaded.contains(&format!("session.message:{id}")) {
            self.store.loaded.insert(format!("session.message:{id}"));
            self.fetch_messages(id);
        }
        if !self.store.loaded.contains(&format!("session.pending:{id}")) {
            self.store.loaded.insert(format!("session.pending:{id}"));
            self.fetch_pending(id);
        }
        // Children (family) and permissions/forms.
        let c = self.client.clone();
        let sid = id.to_string();
        self.spawn(
            async move { c.session_list(&[("parentID", sid.clone()), ("order", "desc".into())]).await },
            |app, r| {
                if let Ok(page) = r {
                    for s in page.data {
                        app.store.remember(s);
                    }
                    app.dirty = true;
                }
            },
        );
        let c = self.client.clone();
        let sid = id.to_string();
        self.spawn(async move { c.session_permissions(&sid).await.map(|p| (sid, p)) }, |app, r| {
            if let Ok((sid, list)) = r {
                let entry = app.store.permissions.entry(sid).or_default();
                for p in list {
                    if !entry.iter().any(|x| x.id == p.id) {
                        entry.push(p);
                    }
                }
                app.dirty = true;
            }
        });
        let c = self.client.clone();
        let sid = id.to_string();
        self.spawn(async move { c.session_forms(&sid).await.map(|p| (sid, p)) }, |app, r| {
            if let Ok((sid, list)) = r {
                let entry = app.store.forms.entry(sid).or_default();
                for f in list {
                    if !entry.iter().any(|(x, _)| x.id == f.id) {
                        entry.push((f, None));
                    }
                }
                app.dirty = true;
            }
        });
    }

    pub fn sync_location(&mut self) {
        let dir = self.location_dir();
        let c = self.client.clone();
        let d2 = dir.clone();
        self.spawn(async move { c.location_get(Some(&d2)).await }, move |app, r| {
            if let Ok(info) = r {
                app.store.location_mut(&dir).info = Some(info);
                app.dirty = true;
            }
        });
        let dir = self.location_dir();
        for cat in [Catalog::Agents, Catalog::Models, Catalog::Providers, Catalog::Commands, Catalog::Skills, Catalog::Mcp, Catalog::References, Catalog::Vcs, Catalog::Shells, Catalog::Integrations] {
            self.fetch_catalog(&dir, cat);
        }
        let c = self.client.clone();
        let d2 = dir.clone();
        self.spawn(async move { c.default_model(Some(&d2)).await }, |app, r| {
            if let Ok(Some(m)) = r {
                app.default_model = Some(ModelRef { provider_id: m.provider_id, id: m.id, variant: None });
                app.dirty = true;
            }
        });
        let c = self.client.clone();
        let d2 = dir.clone();
        self.spawn(async move { c.forms(Some(&d2)).await }, |app, r| {
            if let Ok(list) = r {
                for f in list {
                    let entry = app.store.forms.entry(f.session_id.clone()).or_default();
                    if !entry.iter().any(|(x, _)| x.id == f.id) {
                        entry.push((f, None));
                    }
                }
                app.dirty = true;
            }
        });
        let c = self.client.clone();
        self.spawn(async move { c.permissions(Some(&dir)).await }, |app, r| {
            if let Ok(list) = r {
                for p in list {
                    let entry = app.store.permissions.entry(p.session_id.clone()).or_default();
                    if !entry.iter().any(|x| x.id == p.id) {
                        entry.push(p);
                    }
                }
                app.dirty = true;
            }
        });
    }

    pub fn fetch_catalog(&mut self, dir: &str, cat: Catalog) {
        let c = self.client.clone();
        let d = dir.to_string();
        macro_rules! fetch {
            ($call:ident, $field:ident) => {{
                let d2 = d.clone();
                self.spawn(async move { c.$call(Some(&d2)).await }, move |app, r| {
                    if let Ok(v) = r {
                        app.store.location_mut(&d).$field = Some(v);
                        app.dirty = true;
                    }
                });
            }};
        }
        match cat {
            Catalog::Agents => fetch!(agents, agents),
            Catalog::Models => fetch!(models, models),
            Catalog::Providers => fetch!(providers, providers),
            Catalog::Commands => fetch!(commands, commands),
            Catalog::Skills => fetch!(skills, skills),
            Catalog::Mcp => fetch!(mcp, mcp),
            Catalog::References => fetch!(references, references),
            Catalog::Integrations => fetch!(integrations, integrations),
            Catalog::Vcs => fetch!(vcs, vcs),
            Catalog::Shells => {
                let d2 = d.clone();
                self.spawn(async move { c.shell_list(Some(&d2)).await }, move |app, r| {
                    if let Ok(v) = r {
                        app.store.location_mut(&d).shells = v.into_iter().filter(|s| s.status == "running").collect();
                        app.dirty = true;
                    }
                });
            }
        }
    }

    // ---- event handling ----------------------------------------------------

    pub fn on_sse(&mut self, msg: SseMsg) {
        match msg {
            SseMsg::Status { status, .. } => {
                if status != self.connection {
                    self.connection_since = Instant::now();
                }
                self.connection = status;
                if status == ConnStatus::Connected && !self.ever_connected {
                    self.ever_connected = true;
                }
                self.dirty = true;
            }
            SseMsg::Event(ev) => self.on_event(*ev),
        }
    }

    pub fn on_event(&mut self, ev: Event) {
        let was_connected = ev.kind == "server.connected";
        let current_deleted = ev.kind == "session.deleted" && ev.session_id() == self.session_id();
        let deleted_title = if current_deleted { self.session().and_then(|s| s.title.clone()) } else { None };
        let fx = self.store.apply(&ev);
        self.run_effects(fx);
        self.turn_event(&ev);
        self.dirty = true;
        if was_connected {
            self.on_connected();
        }
        let matches_dir = ev.location.as_ref().is_none_or(|l| l.directory == self.location_dir() || l.directory == self.opts.directory);
        match ev.kind.as_str() {
            "tui.toast.show" if matches_dir => {
                let d = &ev.data;
                let mut t = Toast::new(d.get("variant").and_then(|v| v.as_str()).unwrap_or("info"), d.get("message").and_then(|v| v.as_str()).unwrap_or(""));
                if let Some(title) = d.get("title").and_then(|v| v.as_str()) {
                    t.title = Some(title.into());
                }
                if let Some(dur) = d.get("duration").and_then(|v| v.as_u64()) {
                    t.duration = dur;
                }
                self.toast(t);
            }
            "tui.command.execute" if matches_dir => {
                if let Some(cmd) = ev.data.get("command").and_then(|v| v.as_str()) {
                    self.run_command(cmd, None);
                }
            }
            "tui.session.select" if matches_dir => {
                if let Some(id) = ev.data.get("sessionID").and_then(|v| v.as_str()) {
                    self.navigate(Route::Session(id.to_string()));
                }
            }
            "tui.prompt.append" if matches_dir => {
                if let Some(text) = ev.data.get("text").and_then(|v| v.as_str()) {
                    self.editor.insert(text);
                    self.editor.buffer_end();
                }
            }
            "session.deleted" if current_deleted => {
                self.navigate(Route::Home);
                let msg = match deleted_title {
                    Some(t) => format!("Session \"{t}\" was deleted"),
                    None => "The current session was deleted".to_string(),
                };
                self.info(&msg);
            }
            "session.renamed" => {
                if ev.session_id() == self.session_id() {
                    self.epilogue_update();
                }
            }
            "permission.asked" => {
                if self.autoaccept {
                    let sid = ev.session_id().unwrap_or("").to_string();
                    let rid = ev.str("id").unwrap_or("").to_string();
                    let c = self.client.clone();
                    self.spawn(async move { c.permission_reply(&sid, &rid, "once", None).await }, |_, _| {});
                } else {
                    crate::attention::notify(self, "permission", "Permission needs input", None);
                }
            }
            "form.created" => {
                let title = ev.data.pointer("/form/title").and_then(|t| t.as_str()).map(|s| s.to_string());
                crate::attention::notify(self, "question", "Input needs response", title.as_deref());
            }
            "session.execution.succeeded" | "session.execution.interrupted" => {
                let child = ev.session_id().and_then(|s| self.store.session(s)).is_some_and(|s| s.parent_id.is_some());
                crate::attention::notify(self, if child { "subagent_done" } else { "done" }, "Session done", None);
            }
            "session.execution.failed" => {
                let msg = ev.data.pointer("/error/message").and_then(|m| m.as_str()).unwrap_or("Session failed").to_string();
                crate::attention::notify(self, "error", &msg, None);
                if ev.session_id().and_then(|s| self.store.session(s)).is_none_or(|s| s.parent_id.is_none()) {
                    self.toast(Toast::new("error", msg).title("Session failed"));
                }
            }
            "persistent-pty.added" | "persistent-pty.removed" => {
                if let Some(sid) = ev.session_id() {
                    let sid = sid.to_string();
                    if self.terminal_lists.contains_key(&sid) {
                        self.refresh_terminals(&sid);
                    }
                }
            }
            "mcp.status.changed" => {}
            _ => {}
        }
    }

    fn on_connected(&mut self) {
        self.sync_location();
        let c = self.client.clone();
        self.spawn(async move { c.info().await }, |app, r| {
            app.terminals_available = match r {
                Ok(info) => info.pointer("/capabilities/persistentPty") != Some(&Value::Bool(false)),
                Err(_) => false,
            };
            if app.terminals_available {
                if let Some(sid) = app.session_id().map(|s| s.to_string()) {
                    app.refresh_terminals(&sid);
                }
            }
        });
        if let Some(id) = self.session_id().map(|s| s.to_string()) {
            self.store.loaded.retain(|k| !k.ends_with(&id));
            self.sync_session(&id);
        }
        // Initial session list for the session list dialog and tabs.
        let c = self.client.clone();
        let dir = self.opts.directory.clone();
        self.spawn(
            async move { c.session_list(&[("limit", "50".into()), ("order", "desc".into()), ("parentID", "null".into()), ("directory", dir)]).await },
            |app, r| {
                if let Ok(page) = r {
                    for s in page.data {
                        if !app.store.sessions.contains_key(&s.id) {
                            app.store.remember(s);
                        }
                    }
                    app.dirty = true;
                }
            },
        );
        for t in self.tabs.clone() {
            if !self.store.sessions.contains_key(&t) {
                self.fetch_session(&t);
            }
        }
        self.post_mount();
    }

    /// `--continue` / `--session --fork` post-mount navigation (once).
    fn post_mount(&mut self) {
        if self.prompt_sent {
            return;
        }
        self.prompt_sent = true;
        if let Some(m) = &self.opts.model
            && !m.contains('/')
        {
            let m = m.clone();
            self.toast(Toast::new("warning", format!("Invalid model format: {m}")).duration(3000));
        }
        if self.opts.continue_last {
            let c = self.client.clone();
            let dir = self.opts.directory.clone();
            let fork = self.opts.fork;
            self.spawn(
                async move {
                    let page = c.session_list(&[("limit", "1".into()), ("order", "desc".into()), ("parentID", "null".into()), ("directory", dir)]).await?;
                    let Some(first) = page.data.into_iter().next() else { return Ok(None) };
                    if fork {
                        return Ok(Some(c.session_fork(&first.id, None).await?));
                    }
                    Ok(Some(first))
                },
                |app, r| match r {
                    Ok(Some(s)) => {
                        let id = s.id.clone();
                        app.store.remember(s);
                        app.navigate(Route::Session(id));
                    }
                    Ok(None) => {}
                    Err(e) => app.toast_error(&e),
                },
            );
        } else if let (Some(id), true) = (self.opts.session.clone(), self.opts.fork) {
            let c = self.client.clone();
            self.call(async move { c.session_fork(&id, None).await }, |app, s| {
                let id = s.id.clone();
                app.store.remember(s);
                app.navigate(Route::Session(id));
            });
        }
    }

    /// Periodic work: toasts, leader timeout, interrupt reset, shell polling.
    pub fn tick(&mut self) {
        self.frame += 1;
        if let Some(t) = &self.toast
            && let Some(shown) = t.shown
            && shown.elapsed() >= Duration::from_millis(t.duration)
        {
            self.toast = None;
            if !self.toast_queue.is_empty() {
                let next = self.toast_queue.remove(0);
                self.toast(next);
            }
            self.dirty = true;
        }
        if let Some(d) = self.pending_deadline
            && Instant::now() >= d
        {
            self.pending_keys.clear();
            self.pending_deadline = None;
            self.dirty = true;
        }
        if let Some(at) = self.interrupt_at
            && at.elapsed() >= Duration::from_millis(5000)
        {
            self.interrupt = 0;
            self.interrupt_at = None;
            self.dirty = true;
        }
        self.turn_tick();
        if self.theme_needs_reload() {
            self.reload_theme();
        }
    }

    fn theme_needs_reload(&self) -> bool {
        false
    }

    pub fn reload_theme(&mut self) {
        self.syntax = SyntaxStyle::from_theme(&self.themes.current);
        self.thinking_syntax = SyntaxStyle::muted(&self.themes.current);
        let errors = std::mem::take(&mut self.themes.errors);
        for (name, msg) in errors {
            self.toast(Toast::new("error", msg).title(format!("Failed to load theme: {name}")));
        }
        self.dirty = true;
    }

    pub fn set_theme(&mut self, name: &str, persist: bool) -> bool {
        if !self.themes.set(name) {
            return false;
        }
        if persist {
            self.config.set("theme.name", Some(Value::String(name.into())));
        }
        self.reload_theme();
        true
    }

    /// Whether any animation needs periodic redraws.
    pub fn animating(&self) -> bool {
        self.running() || self.toast.is_some() || self.pending_deadline.is_some() || self.connection != ConnStatus::Connected
            || self.migration.is_some()
            || self.store.active.values().any(|v| *v)
            || !self.busy.is_empty()
    }

    pub fn spinner_frame(&self) -> &'static str {
        let i = (self.start.elapsed().as_millis() / 80) as usize % 10;
        crate::ui::text::SPINNER_FRAMES[i]
    }

    pub fn now(&self) -> i64 {
        now_ms()
    }
}

#[cfg(test)]
mod tests;
