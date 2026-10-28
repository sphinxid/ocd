//! The mini footer (`src/mini/footer.ts` `RunFooter` + `footer.view.tsx` `RunFooterView`): the
//! scrollback commit queue, footer state and notices, the height policy, key routing, and the
//! footer frame (startup row, composer or panel or blocker, autocomplete or statusline).

use super::blocker::{FormBody, FormOutcome, PermissionBody};
use super::cell::{Buffer, Color, Glyph, StyledChunk};
use super::footer_width::{Action, GroupId, StatusInput, Tone, footer_statusline_policy};
use super::menu::{FOOTER_MENU_ROWS, MenuView};
use super::mono::mono_shortcut;
use super::motion;
use super::panels::{self, Panel, PanelAction, PanelData, PanelKind, PanelOutcome, RUN_SUBAGENT_PANEL_ROWS, footer_panel_layout};
use super::permission_shared::PermissionReply;
use super::form_shared::FormReply;
use super::prompt::{Composer, Host, PromptLayout, TEXTAREA_MIN_ROWS, footer_prompt_layout};
use super::scrollback::{Commit, ScrollbackStream, Sink};
use super::splash;
use super::split::CursorReq;
use super::subagent_view::{SUBAGENT_INSPECTOR_ROWS, SubagentInspector};
use super::text::{WrapMode, layout};
use super::theme::RunTheme;
use super::types::*;
use super::variant::model_info;
use crate::keymap::{Keybinds, Match, Stroke};
use crate::util::locale::{self, width as sw};
use std::time::{Duration, Instant};

const PERMISSION_ROWS: usize = 12;
const FORM_ROWS: usize = 14;
const NOTICE_DURATION: Duration = Duration::from_millis(3000);

/// `FooterEvent`.
#[derive(Clone, Debug)]
pub enum FooterEvent {
    First(bool),
    /// `{ type: "model", model, selection }`.
    Model { label: String, selection: Option<ModelSel> },
    TurnSend,
    TurnIdle,
    TurnDuration(String),
    Patch(FooterPatch),
    View(FooterView),
    Subagent(SubagentState),
    History(Vec<RunPrompt>),
    Agent(Option<String>),
    Catalog { agents: Vec<RunAgent>, references: Vec<RunReference>, commands: Option<Vec<RunCommand>> },
    Models(Vec<RunProvider>),
    Variants { variants: Vec<String>, current: Option<String> },
    QueuedPrompts(Vec<QueuedPrompt>),
}

/// Requests from the footer to the runtime.
#[derive(Clone, Debug, PartialEq)]
pub enum FooterAction {
    Prompt(RunPrompt),
    PermissionReply(PermissionReply),
    FormReply(FormReply),
    FormCancel { session_id: String, form_id: String, location: Option<String> },
    CycleVariant,
    AgentSelect(String),
    ModelSelect(ModelSel),
    VariantSelect(Option<String>),
    Interrupt,
    Background,
    QueuedAction(QueuedPromptAction, String),
    EditorOpen(String),
    SubagentSelect(Option<String>),
    SubagentInterrupt(String),
    MiniSetting(MiniSettingChange),
    FindFiles(String),
    ClearScreen,
    Paste,
    /// The footer closed (exit requested).
    Close,
}

pub struct FooterOptions {
    pub agents: Vec<RunAgent>,
    pub references: Vec<RunReference>,
    pub agent: Option<String>,
    pub model_label: String,
    pub model: Option<ModelSel>,
    pub variant: Option<String>,
    pub first: bool,
    pub history: Vec<RunPrompt>,
    pub theme: RunTheme,
    pub settings: MiniSettings,
    pub keybinds: Keybinds,
    pub animations: bool,
    pub cursor_style: u8,
    pub wrote: bool,
    pub startup: Option<(String, String)>,
    pub directory: String,
    pub image_preview: bool,
}

pub struct Footer {
    pub closed: bool,
    pub destroyed: bool,
    queue: Vec<StreamCommit>,
    flushed: Vec<StreamCommit>,
    pending_summary: Vec<(String, String, String)>,
    pending_complete: bool,
    pub scrollback: ScrollbackStream,
    splash: Vec<Commit>,
    rows: usize,
    base: usize,
    agents: Vec<RunAgent>,
    references: Vec<RunReference>,
    commands: Option<Vec<RunCommand>>,
    providers: Option<Vec<RunProvider>>,
    selected_agent: Option<String>,
    pub current_model: Option<ModelSel>,
    variants: Vec<String>,
    current_variant: Option<String>,
    pub theme: RunTheme,
    pub state: FooterState,
    pub view: FooterView,
    pub subagent: SubagentState,
    queued: Vec<QueuedPrompt>,
    pub settings: MiniSettings,
    route: PromptRoute,
    subagent_menu_rows: usize,
    interrupt_deadline: Option<Instant>,
    exit_deadline: Option<Instant>,
    notice_deadline: Option<Instant>,
    turn_agent: Option<String>,
    pub startup: Option<(String, String)>,
    startup_at: Instant,
    pub keys: Keybinds,
    pending: Vec<Stroke>,
    pending_at: Option<Instant>,
    animations: bool,
    cursor_style: u8,
    pub directory: String,
    composer: Composer,
    panel: Option<Panel>,
    permission: Option<PermissionBody>,
    form: Option<FormBody>,
    inspector: SubagentInspector,
    busy_since: Option<Instant>,
    pub actions: Vec<FooterAction>,
    pub term_width: usize,
    pub term_height: usize,
    /// Requested renderer footer height.
    pub height: usize,
    now: Instant,
}

/// `resolveRunAgent(agents, current)`.
pub fn resolve_run_agent<'a>(agents: &'a [RunAgent], current: Option<&str>) -> Option<&'a RunAgent> {
    let mut selectable = agents.iter().filter(|a| a.mode != "subagent" && !a.hidden);
    match current {
        None => selectable.next(),
        Some(c) => selectable.find(|a| a.id == c),
    }
}

/// `coalesceProgressCommit(previous, current)`.
pub fn coalesce_progress_commit(previous: &StreamCommit, current: &StreamCommit) -> Option<StreamCommit> {
    if previous.phase != Phase::Progress
        || current.phase != Phase::Progress
        || previous.kind != current.kind
        || previous.source != current.source
        || previous.message_id != current.message_id
        || previous.part_id != current.part_id
        || previous.tool != current.tool
        || previous.tool_state != current.tool_state
    {
        return None;
    }
    Some(StreamCommit { text: format!("{}{}", previous.text, current.text), ..current.clone() })
}

fn tool_transcript(c: &StreamCommit) -> bool {
    c.kind == EntryKind::Tool || c.part_id.as_deref().is_some_and(|p| p.starts_with("skill:"))
}

/// `formatContextUsage(tokens, percent)`.
fn format_context_usage(tokens: f64, percent: Option<i64>) -> String {
    let value = locale::number(tokens);
    match percent {
        None => value,
        Some(p) => format!("{value} ({p}%)"),
    }
}

impl Footer {
    pub fn new(o: FooterOptions, term_width: usize, term_height: usize, footer_height: usize, now: Instant) -> Footer {
        let scrollback = ScrollbackStream::new(o.theme.clone(), o.wrote, o.settings.mono, o.settings.shell_output, o.image_preview);
        Footer {
            closed: false,
            destroyed: false,
            queue: Vec::new(),
            flushed: Vec::new(),
            pending_summary: Vec::new(),
            pending_complete: false,
            scrollback,
            splash: Vec::new(),
            rows: TEXTAREA_MIN_ROWS,
            base: footer_height.saturating_sub(TEXTAREA_MIN_ROWS).max(1),
            agents: o.agents,
            references: o.references,
            commands: None,
            providers: None,
            selected_agent: o.agent,
            current_model: o.model,
            variants: Vec::new(),
            current_variant: o.variant,
            theme: o.theme,
            state: FooterState { phase: FooterPhase::Idle, status: String::new(), notice: String::new(), model: o.model_label, usage: None, first: o.first, interrupt: 0, exit: 0 },
            view: FooterView::Prompt,
            subagent: SubagentState::default(),
            queued: Vec::new(),
            settings: o.settings,
            route: PromptRoute::Composer,
            subagent_menu_rows: RUN_SUBAGENT_PANEL_ROWS,
            interrupt_deadline: None,
            exit_deadline: None,
            notice_deadline: None,
            turn_agent: None,
            startup: o.startup,
            startup_at: now,
            keys: o.keybinds,
            pending: Vec::new(),
            pending_at: None,
            animations: o.animations,
            cursor_style: o.cursor_style,
            directory: o.directory,
            composer: Composer::new(&o.history),
            panel: None,
            permission: None,
            form: None,
            inspector: SubagentInspector::default(),
            busy_since: None,
            actions: Vec::new(),
            term_width,
            term_height,
            height: footer_height,
            now,
        }
    }

    pub fn is_closed(&self) -> bool {
        self.closed || self.destroyed
    }

    // --- agents / models -----------------------------------------------------------------------

    pub fn current_agent_id(&self) -> Option<String> {
        resolve_run_agent(&self.agents, self.selected_agent.as_deref()).map(|a| a.id.clone()).or(self.selected_agent.clone())
    }

    /// `currentAgent()`: the display name.
    pub fn current_agent(&self) -> String {
        if let Some(a) = resolve_run_agent(&self.agents, self.selected_agent.as_deref()) {
            return a.name.clone();
        }
        match &self.selected_agent {
            Some(s) => locale::titlecase(s),
            None => "Default".into(),
        }
    }

    fn agent_color(&self) -> Color {
        let colors = &self.theme.footer.categorical;
        let id = self.current_agent_id();
        let index = self.agents.iter().filter(|a| !a.hidden).position(|a| Some(&a.id) == id.as_ref()).unwrap_or(0);
        colors[index % colors.len().max(1)]
    }

    // --- startup -------------------------------------------------------------------------------

    /// `finishStartup()`: drop the startup row and write the entry splash.
    pub fn finish_startup(&mut self) {
        let Some((version, detail)) = self.startup.take() else { return };
        if self.destroyed {
            return;
        }
        self.apply_height();
        self.splash.push(splash::entry_splash(self.term_width, &version, Some(&detail), self.settings.mono, &self.theme.splash));
    }

    /// Startup animation length: `(SEED_LAUNCH.frames.length - 1) * interval`.
    pub fn startup_due(&self, now: Instant) -> bool {
        self.startup.is_some() && now.duration_since(self.startup_at) >= Duration::from_millis(800)
    }

    // --- events --------------------------------------------------------------------------------

    pub fn event(&mut self, next: FooterEvent) {
        match next {
            FooterEvent::History(h) => {
                self.composer.set_history(&h);
            }
            FooterEvent::Agent(a) => self.selected_agent = a,
            FooterEvent::Model { label, selection } => {
                self.current_model = selection;
                self.patch(FooterPatch { model: Some(label), ..Default::default() });
            }
            FooterEvent::TurnDuration(duration) => {
                self.finish_startup();
                let agent = self.turn_agent.take().unwrap_or_else(|| self.current_agent());
                if !self.settings.turn_summary {
                    return;
                }
                let model = match &self.current_model {
                    Some(m) => model_info(self.providers.as_deref(), m).1,
                    None => self.state.model.clone(),
                };
                self.flush_queue();
                self.pending_summary.push((agent, model, duration));
            }
            FooterEvent::Catalog { agents, references, commands } => {
                if self.destroyed {
                    return;
                }
                self.agents = agents;
                self.references = references;
                if commands.is_some() {
                    self.commands = commands;
                }
            }
            FooterEvent::Models(p) => {
                if !self.destroyed {
                    self.providers = Some(p);
                }
            }
            FooterEvent::Variants { variants, current } => {
                if !self.destroyed {
                    self.variants = variants;
                    self.current_variant = current;
                }
            }
            FooterEvent::QueuedPrompts(p) => {
                if !self.destroyed {
                    self.queued = p;
                }
            }
            FooterEvent::First(first) => self.patch(FooterPatch { first: Some(first), ..Default::default() }),
            FooterEvent::TurnSend => {
                self.turn_agent = Some(self.current_agent());
                self.interrupt_deadline = None;
                self.exit_deadline = None;
                self.notice_deadline = None;
                self.patch(FooterPatch { phase: Some(FooterPhase::Running), status: Some("sending prompt".into()), notice: Some(String::new()), interrupt: Some(0), exit: Some(0), ..Default::default() });
            }
            FooterEvent::TurnIdle => {
                self.notice_deadline = None;
                self.patch(FooterPatch { phase: Some(FooterPhase::Idle), status: Some(String::new()), notice: Some(String::new()), ..Default::default() });
            }
            FooterEvent::Patch(mut p) => {
                if p.status.is_some() {
                    self.notice_deadline = None;
                    p.notice = Some(String::new());
                }
                self.patch(p);
            }
            FooterEvent::Subagent(s) => {
                if self.destroyed {
                    return;
                }
                self.subagent = s;
                self.sync_routes();
                self.apply_height();
            }
            FooterEvent::View(v) => self.present(v),
        }
    }

    fn patch(&mut self, next: FooterPatch) {
        if self.destroyed {
            return;
        }
        let prev = self.state.clone();
        let mut state = FooterState {
            phase: next.phase.unwrap_or(prev.phase),
            status: next.status.unwrap_or(prev.status.clone()),
            notice: next.notice.unwrap_or(prev.notice.clone()),
            model: next.model.unwrap_or(prev.model.clone()),
            usage: match next.usage {
                Some(u) => u,
                None => prev.usage.clone(),
            },
            first: next.first.unwrap_or(prev.first),
            interrupt: next.interrupt.unwrap_or(prev.interrupt),
            exit: next.exit.unwrap_or(prev.exit),
        };
        if state.phase == FooterPhase::Idle {
            state.interrupt = 0;
        }
        if prev.phase != state.phase {
            self.busy_since = (state.phase == FooterPhase::Running).then_some(self.now);
        }
        let was_running = prev.phase == FooterPhase::Running;
        self.state = state;
        if was_running && self.state.phase == FooterPhase::Idle {
            self.flush_queue();
            self.pending_complete = true;
        }
    }

    fn present(&mut self, view: FooterView) {
        if self.destroyed {
            return;
        }
        if !matches!(view, FooterView::Prompt) {
            self.finish_startup();
        }
        match &view {
            FooterView::Permission(p) => {
                if self.permission.as_ref().is_none_or(|b| b.request_id() != p.request.id) {
                    self.permission = Some(PermissionBody::new(p));
                }
            }
            FooterView::Form(f) => {
                if self.form.as_ref().is_none_or(|b| b.form_id() != f.form.id) {
                    self.form = Some(FormBody::new(f));
                }
            }
            FooterView::Prompt => {}
        }
        self.view = view;
        self.composer.view_changed(&self.prompt_view());
        self.sync_routes();
        self.apply_height();
    }

    /// `append(commit)`.
    pub fn append(&mut self, commit: StreamCommit) {
        if self.destroyed {
            return;
        }
        self.finish_startup();
        if !self.settings.tools && tool_transcript(&commit) {
            return;
        }
        if let Some(last) = self.queue.last()
            && let Some(merged) = coalesce_progress_commit(last, &commit)
        {
            *self.queue.last_mut().unwrap() = merged;
            return;
        }
        self.queue.push(commit);
    }

    fn flush_queue(&mut self) {
        let batch = std::mem::take(&mut self.queue);
        self.flushed.extend(batch);
    }

    /// Drain everything pending into the renderer (`flush()` + the queued writer tasks): the entry
    /// splash, scrollback commits, a turn summary and the completion of the active entry.
    pub fn drain(&mut self, sink: &mut dyn Sink) {
        if self.destroyed {
            self.queue.clear();
            return;
        }
        for c in self.splash.drain(..) {
            sink.commit(c);
        }
        self.flush_queue();
        for c in std::mem::take(&mut self.flushed) {
            self.scrollback.append(sink, c);
        }
        for (agent, model, duration) in std::mem::take(&mut self.pending_summary) {
            self.scrollback.write_turn_summary(sink, &agent, &model, &duration);
        }
        if std::mem::take(&mut self.pending_complete) {
            self.scrollback.complete(sink, false);
        }
    }

    /// `idle()`: like [`Footer::drain`] and completes the active entry when idle.
    pub fn idle(&mut self, sink: &mut dyn Sink) {
        self.drain(sink);
        if self.state.phase == FooterPhase::Idle {
            self.scrollback.complete(sink, false);
        }
    }

    pub fn has_pending(&self) -> bool {
        !self.queue.is_empty() || !self.flushed.is_empty() || !self.splash.is_empty() || !self.pending_summary.is_empty() || self.pending_complete
    }

    /// `resetForReplay(wrote)`.
    pub fn reset_for_replay(&mut self, wrote: bool) {
        if self.destroyed {
            return;
        }
        self.finish_startup();
        self.scrollback.destroy();
        self.scrollback = ScrollbackStream::new(self.theme.clone(), wrote, self.settings.mono, self.settings.shell_output, self.scrollback.image_preview);
    }

    pub fn close(&mut self) {
        if self.closed {
            return;
        }
        self.finish_startup();
        self.flush_queue();
        self.closed = true;
        self.actions.push(FooterAction::Close);
    }

    pub fn destroy(&mut self) {
        if self.destroyed {
            return;
        }
        self.flush_queue();
        self.destroyed = true;
        if !self.closed {
            self.closed = true;
            self.actions.push(FooterAction::Close);
        }
        self.scrollback.destroy();
    }

    /// `requestExit()` (SIGINT).
    pub fn request_exit(&mut self) -> bool {
        if self.prompt_active() || matches!(self.view, FooterView::Permission(_) | FooterView::Form(_)) || self.base_bindings_enabled() {
            let active = self.prompt_active();
            let mut c = std::mem::take(&mut self.composer);
            let r = c.request_exit(self, active);
            self.composer = c;
            return r;
        }
        self.handle_exit()
    }

    pub fn set_notice(&mut self, text: String) {
        self.notice_deadline = None;
        let empty = text.is_empty();
        self.patch(FooterPatch { notice: Some(text), ..Default::default() });
        if !empty {
            self.notice_deadline = Some(self.now + NOTICE_DURATION);
        }
    }

    fn handle_input_clear(&mut self) {
        self.interrupt_deadline = None;
        self.exit_deadline = None;
        if self.state.interrupt == 0 && self.state.exit == 0 {
            return;
        }
        self.patch(FooterPatch { interrupt: Some(0), exit: Some(0), ..Default::default() });
    }

    /// `handleInterrupt` (two-press).
    fn handle_interrupt(&mut self) -> bool {
        if self.is_closed() || self.state.phase != FooterPhase::Running {
            return false;
        }
        let next = self.state.interrupt + 1;
        self.patch(FooterPatch { interrupt: Some(next), ..Default::default() });
        if next < 2 {
            self.interrupt_deadline = Some(self.now + Duration::from_millis(5000));
            return true;
        }
        self.interrupt_deadline = None;
        self.patch(FooterPatch { interrupt: Some(0), ..Default::default() });
        self.set_notice("interrupting".into());
        self.actions.push(FooterAction::Interrupt);
        true
    }

    /// `handleExit` (two-press).
    fn handle_exit(&mut self) -> bool {
        if self.is_closed() {
            return true;
        }
        self.interrupt_deadline = None;
        let next = self.state.exit + 1;
        self.patch(FooterPatch { exit: Some(next), interrupt: Some(0), ..Default::default() });
        if next < 2 {
            self.exit_deadline = Some(self.now + Duration::from_millis(5000));
            return true;
        }
        self.exit_deadline = None;
        self.patch(FooterPatch { exit: Some(0), status: Some("exiting".into()), ..Default::default() });
        self.close();
        true
    }

    fn handle_prompt(&mut self, input: RunPrompt) -> bool {
        if self.is_closed() {
            return false;
        }
        if self.state.first {
            self.patch(FooterPatch { first: Some(false), ..Default::default() });
        }
        self.actions.push(FooterAction::Prompt(input));
        true
    }

    /// Selection result of a model/variant change (`applySelectionResult`).
    pub fn apply_selection(&mut self, variants: Option<Vec<String>>, variant: Option<Option<String>>, model_label: Option<String>, status: Option<String>) {
        if let Some(v) = variants {
            self.variants = v;
        }
        if let Some(v) = variant {
            self.current_variant = v;
        }
        if let Some(l) = model_label {
            self.patch(FooterPatch { model: Some(l), ..Default::default() });
        }
        if let Some(s) = status {
            self.set_notice(s);
        }
    }

    /// `handleCycle` result: `None` → "no variants available".
    pub fn cycle_result(&mut self, result: Option<(Option<String>, String, Option<String>)>) {
        match result {
            None => self.set_notice("no variants available".into()),
            Some((variant, label, status)) => {
                let had_status = status.is_some();
                self.apply_selection(None, Some(variant), Some(label), status);
                if !had_status {
                    self.set_notice("variant updated".into());
                }
            }
        }
    }

    fn handle_model_select(&mut self, model: ModelSel) {
        if self.is_closed() {
            return;
        }
        let previous = self.current_model.clone();
        if previous.as_ref() != Some(&model) {
            self.current_variant = None;
        }
        self.current_model = Some(model.clone());
        self.actions.push(FooterAction::ModelSelect(model));
    }

    fn handle_agent_select(&mut self, agent: String) {
        if self.is_closed() || self.current_agent_id().as_deref() == Some(agent.as_str()) {
            return;
        }
        self.selected_agent = Some(agent.clone());
        self.actions.push(FooterAction::AgentSelect(agent));
        let name = self.current_agent();
        self.set_notice(format!("agent {name}"));
    }

    /// Mini settings were saved: apply them (`handleMiniSettingChange`).
    pub fn settings_saved(&mut self, settings: MiniSettings, theme: Option<RunTheme>) {
        if let Some(p) = self.panel.as_mut() {
            p.saving = None;
        }
        if self.is_closed() {
            return;
        }
        let mono_changed = settings.mono != self.settings.mono;
        self.settings = settings;
        self.scrollback.shell_output = self.settings.shell_output;
        if mono_changed {
            self.scrollback.mono = self.settings.mono;
            if let Some(t) = theme {
                self.theme = t.clone();
                self.scrollback.set_theme(t);
            }
        }
        self.set_notice("settings updated".into());
    }

    pub fn settings_failed(&mut self) {
        if let Some(p) = self.panel.as_mut() {
            p.saving = None;
        }
        self.set_notice("failed to save settings".into());
    }

    pub fn set_theme(&mut self, theme: RunTheme) {
        if theme == self.theme {
            return;
        }
        self.theme = theme.clone();
        self.scrollback.set_theme(theme);
    }

    /// Timers (notice, two-press windows, leader timeout, startup).
    pub fn tick(&mut self, now: Instant) {
        self.now = now;
        if self.notice_deadline.is_some_and(|d| now >= d) {
            self.notice_deadline = None;
            self.patch(FooterPatch { notice: Some(String::new()), ..Default::default() });
        }
        if self.interrupt_deadline.is_some_and(|d| now >= d) {
            self.interrupt_deadline = None;
            if self.state.phase == FooterPhase::Running {
                self.patch(FooterPatch { interrupt: Some(0), ..Default::default() });
            }
        }
        if self.exit_deadline.is_some_and(|d| now >= d) {
            self.exit_deadline = None;
            if !self.is_closed() {
                self.patch(FooterPatch { exit: Some(0), ..Default::default() });
            }
        }
        if self.pending_at.is_some_and(|t| now.duration_since(t) >= Duration::from_millis(self.keys.leader_timeout_ms)) {
            self.pending.clear();
            self.pending_at = None;
        }
        if self.startup_due(now) {
            self.finish_startup();
        }
    }

    /// The next instant a timer or animation needs a frame.
    pub fn next_wake(&self) -> Option<Instant> {
        let mut t: Vec<Instant> = [self.notice_deadline, self.interrupt_deadline, self.exit_deadline].into_iter().flatten().collect();
        if let Some(p) = self.pending_at {
            t.push(p + Duration::from_millis(self.keys.leader_timeout_ms));
        }
        if self.startup.is_some() {
            t.push(self.now + Duration::from_millis(40));
        }
        if self.state.phase == FooterPhase::Running && self.animations {
            t.push(self.now + Duration::from_millis(40));
        }
        t.into_iter().min()
    }

    // --- routing -------------------------------------------------------------------------------

    pub fn prompt_active(&self) -> bool {
        matches!(self.view, FooterView::Prompt) && self.route == PromptRoute::Composer
    }

    fn panel_open(&self) -> bool {
        matches!(self.view, FooterView::Permission(_) | FooterView::Form(_))
            || (matches!(self.view, FooterView::Prompt) && matches!(self.route, PromptRoute::QueuedMenu | PromptRoute::SubagentMenu | PromptRoute::Command | PromptRoute::Agent | PromptRoute::Model | PromptRoute::Variant | PromptRoute::Settings))
    }

    fn inspecting(&self) -> bool {
        matches!(self.view, FooterView::Prompt) && matches!(self.route, PromptRoute::Subagent(_))
    }

    /// `promptView()`.
    fn prompt_view(&self) -> String {
        match &self.view {
            FooterView::Prompt => match self.route {
                PromptRoute::Composer => "prompt".into(),
                _ => self.route.name().into(),
            },
            v => v.kind().into(),
        }
    }

    fn base_bindings_enabled(&self) -> bool {
        !matches!(self.prompt_view().as_str(), "command" | "skill" | "model" | "variant" | "settings" | "queued-menu" | "subagent-menu")
    }

    fn open_route(&mut self, next: PromptRoute) {
        self.panel = match &next {
            PromptRoute::Command => Some(Panel::new(PanelKind::Command)),
            PromptRoute::Agent => Some(Panel::new(PanelKind::Agent)),
            PromptRoute::Model => Some(Panel::new(PanelKind::Model)),
            PromptRoute::Variant => Some(Panel::new(PanelKind::Variant)),
            PromptRoute::Settings => Some(Panel::new(PanelKind::Settings)),
            PromptRoute::SubagentMenu => Some(Panel::new(PanelKind::Subagent)),
            PromptRoute::QueuedMenu => Some(Panel::new(PanelKind::Queued)),
            _ => None,
        };
        if next != PromptRoute::Composer {
            self.finish_startup();
        }
        self.route = next;
        self.actions.push(FooterAction::SubagentSelect(None));
        self.composer.view_changed(&self.prompt_view());
        self.sync_layout();
    }

    fn close_panel(&mut self) {
        self.panel = None;
        self.route = PromptRoute::Composer;
        self.composer.view_changed(&self.prompt_view());
        self.sync_layout();
    }

    fn open_tab(&mut self, session_id: String) {
        self.panel = None;
        self.route = PromptRoute::Subagent(session_id.clone());
        self.inspector.reset();
        self.actions.push(FooterAction::SubagentSelect(Some(session_id)));
        self.composer.view_changed(&self.prompt_view());
        self.finish_startup();
        self.sync_layout();
    }

    fn cycle_tab(&mut self, dir: i64) {
        let tabs = &self.subagent.tabs;
        if tabs.is_empty() {
            return;
        }
        let current = match &self.route {
            PromptRoute::Subagent(id) => tabs.iter().position(|t| &t.session_id == id).map(|i| i as i64).unwrap_or(-1),
            _ => -1,
        };
        let n = tabs.len() as i64;
        let index = if current == -1 { 0 } else { (current + dir + n) % n };
        let id = tabs[index as usize].session_id.clone();
        self.open_tab(id);
    }

    /// Route effects: close menus whose data went away and panels when a blocker shows.
    fn sync_routes(&mut self) {
        if let PromptRoute::Subagent(id) = &self.route
            && !self.subagent.tabs.iter().any(|t| &t.session_id == id)
        {
            self.route = PromptRoute::Composer;
            self.actions.push(FooterAction::SubagentSelect(None));
            self.composer.view_changed(&self.prompt_view());
        }
        if self.route == PromptRoute::SubagentMenu && self.subagent.tabs.is_empty() {
            self.close_panel();
        }
        if self.route == PromptRoute::QueuedMenu && self.queued.is_empty() {
            self.close_panel();
        }
        if !matches!(self.view, FooterView::Prompt) && matches!(self.route, PromptRoute::Command | PromptRoute::Agent | PromptRoute::Model | PromptRoute::Variant | PromptRoute::Settings | PromptRoute::QueuedMenu | PromptRoute::SubagentMenu) {
            self.close_panel();
        }
    }

    fn sync_layout(&mut self) {
        if self.route != PromptRoute::Composer {
            self.finish_startup();
        }
        if matches!(self.view, FooterView::Prompt) {
            self.apply_height();
        }
    }

    // --- height --------------------------------------------------------------------------------

    fn status_rows(&self) -> usize {
        let text = self.status_sections().iter().map(|g| g.parts.iter().map(|p| p.0.as_str()).collect::<String>()).collect::<Vec<_>>().join(if self.settings.mono { " - " } else { " · " });
        let w = self.status_width();
        if sw(&text) <= w && !text.contains('\n') {
            return 1;
        }
        layout(&[StyledChunk::plain(text, Color::WHITE)], w, WrapMode::Word, false).len().max(1)
    }

    fn prompt_layout(&self) -> PromptLayout {
        self.composer.layout(self, self.term_width)
    }

    /// `onRows`: `promptRows + statusRows - 1` (status rows only with the statusline shown).
    fn sync_rows(&mut self) {
        self.composer.placeholder = self.composer.placeholder(self.state.first, self.settings.mono);
        self.composer.width = self.term_width.saturating_sub(4).max(1);
        self.composer.sync_rows();
        let menu = self.prompt_active() && self.composer.visible();
        self.composer.status_rows = if menu { 0 } else { self.status_rows() };
        let prompt_rows = self.prompt_layout().rows;
        let status = if !self.panel_open() && !menu && !self.inspecting() { self.status_rows() } else { 0 };
        let value = (prompt_rows + status) as i64 - 1;
        let rows = (value.max(TEXTAREA_MIN_ROWS as i64)) as usize;
        if rows != self.rows {
            self.rows = rows;
        }
    }

    /// `applyHeight()`.
    pub fn apply_height(&mut self) {
        self.sync_rows();
        let panel = footer_panel_layout(self.term_height, panels::PANEL_LIST_ROWS);
        let prompt = footer_prompt_layout(self.term_height, TEXTAREA_MIN_ROWS, 0, 1, false);
        let desired = match &self.view {
            FooterView::Permission(_) => self.base + PERMISSION_ROWS,
            FooterView::Form(_) => self.base + FORM_ROWS,
            FooterView::Prompt => match &self.route {
                PromptRoute::Command | PromptRoute::Agent | PromptRoute::Model | PromptRoute::Variant | PromptRoute::Settings => 1 + panel.frame + panel.limit,
                PromptRoute::QueuedMenu | PromptRoute::SubagentMenu => 1 + self.subagent_menu_rows,
                PromptRoute::Subagent(_) => self.base + SUBAGENT_INSPECTOR_ROWS,
                PromptRoute::Composer => prompt.padding * 2 + 1 + self.rows,
            },
        };
        let composer = matches!(self.view, FooterView::Prompt) && self.route == PromptRoute::Composer;
        let height = (desired + usize::from(self.startup.is_some())).min(self.term_height.saturating_sub(usize::from(composer))).max(1);
        self.height = height;
    }

    pub fn resize(&mut self, width: usize, height: usize) {
        self.term_width = width;
        self.term_height = height;
        if !self.destroyed {
            self.apply_height();
        }
    }

    // --- statusline ----------------------------------------------------------------------------

    fn shortcut(&self, id: &str) -> String {
        mono_shortcut(&self.keys.shortcut(id), self.settings.mono)
    }

    fn busy(&self) -> bool {
        self.state.phase == FooterPhase::Running
    }

    fn status_width(&self) -> usize {
        self.term_width.saturating_sub(if self.busy() { 2 } else { 0 }).max(1)
    }

    fn interrupt_label(&self) -> Option<String> {
        let k = self.shortcut("session.interrupt");
        if k.is_empty() {
            return None;
        }
        Some(if k == "escape" { "esc".into() } else { k })
    }

    fn context_usage(&self) -> String {
        match &self.state.usage {
            Some(u) if u.tokens > 0.0 => format_context_usage(u.tokens, u.percent),
            _ => String::new(),
        }
    }

    fn cost(&self) -> String {
        match self.state.usage.as_ref().and_then(|u| u.cost) {
            Some(c) if c != 0.0 => super::util::money(c),
            _ => String::new(),
        }
    }

    fn takeover(&self) -> bool {
        self.state.exit > 0 || (self.busy() && self.state.interrupt > 0) || !self.state.notice.trim().is_empty()
    }

    fn footer_details(&self) -> bool {
        self.settings.footer && !self.takeover()
    }

    fn shell(&self) -> bool {
        self.prompt_active() && self.composer.shell
    }

    fn active_tabs(&self) -> Vec<&SubagentTab> {
        self.subagent.tabs.iter().filter(|t| t.status == SubagentStatus::Running).collect()
    }

    fn status_text(&self) -> String {
        let exiting = self.state.exit > 0;
        let armed = self.state.interrupt > 0;
        if exiting || (self.busy() && armed) {
            let key = if exiting { Some(self.shortcut("prompt.clear")).filter(|k| !k.is_empty()) } else { self.interrupt_label() };
            let action = if exiting { "exit" } else { "stop" };
            let Some(key) = key else { return if exiting { "Exit pending" } else { "Stop pending" }.into() };
            let verb = if exiting { "exit" } else { "interrupt" };
            let phrases = [format!("Press {key} again to {verb}"), format!("{key} again to {verb}"), format!("{key} again: {action}"), format!("{key} {action}")];
            let w = self.status_width();
            return phrases.iter().find(|p| sw(p) <= w).cloned().unwrap_or(phrases[3].clone());
        }
        let notice = self.state.notice.trim();
        if !notice.is_empty() {
            return notice.to_string();
        }
        if !self.footer_details() {
            return if self.shell() { "Shell".into() } else { String::new() };
        }
        let status = self.state.status.trim();
        if self.busy() {
            if status == "reconnecting" {
                return "reconnecting".into();
            }
            return match self.interrupt_label() {
                Some(l) => format!("{l} stop"),
                None => "Running".into(),
            };
        }
        if !status.is_empty() {
            return status.to_string();
        }
        if self.shell() { "Shell".into() } else { String::new() }
    }

    fn status_color(&self) -> Color {
        let t = &self.theme.footer;
        if self.state.exit > 0 {
            return t.error;
        }
        if self.state.interrupt > 0 {
            return t.warning;
        }
        if self.busy() || !self.state.notice.trim().is_empty() || !self.state.status.trim().is_empty() {
            return t.text;
        }
        t.muted
    }

    fn status_sections(&self) -> Vec<super::footer_width::Group> {
        let details = self.footer_details();
        let prompt = self.prompt_active();
        let shell = self.shell();
        let model = self.current_model.as_ref().map(|m| model_info(self.providers.as_deref(), m));
        let current = model.as_ref().map(|m| m.1.clone()).unwrap_or_else(|| self.state.model.trim().to_string());
        let model_status = if !details || !prompt || shell || current.is_empty() { None } else { Some((current.clone(), self.current_variant.clone())) };
        let mut work = Vec::new();
        if details && prompt && !shell {
            let q = self.shortcut("session.queued_prompts");
            if !self.queued.is_empty() && !q.is_empty() {
                work.push((GroupId::Queued, Action { key: q, label: format!("{} pending", self.queued.len()), expanded: None }));
            }
            let active = self.active_tabs().len();
            let s = self.shortcut("session.child.first");
            if active > 0 && !s.is_empty() {
                work.push((GroupId::Subagents, Action { key: s, label: format!("{active} sub"), expanded: Some(format!("{active} subagent{}", if active == 1 { "" } else { "s" })) }));
            }
            let b = self.shortcut("session.background");
            if self.active_tabs().iter().any(|t| !t.background) && !b.is_empty() {
                work.push((GroupId::Background, Action { key: b, label: "bg".into(), expanded: Some("background".into()) }));
            }
        }
        let command = self.shortcut("command.palette.show");
        let menu = if !prompt || self.takeover() || shell || command.is_empty() { None } else { Some(Action { key: command, label: "menu".into(), expanded: None }) };
        let usage = self.context_usage();
        let input = StatusInput {
            width: self.term_width,
            mono: self.settings.mono,
            status: Some((self.status_text(), if details && self.busy() { self.interrupt_label().map(|l| format!("{l} interrupt")) } else { None })),
            escape: if shell && !self.takeover() { Some(Action { key: "esc".into(), label: "normal".into(), expanded: None }) } else { None },
            work,
            model: model_status,
            agent: if !details || !prompt || shell { None } else { Some(self.current_agent()) },
            context: if details && !usage.is_empty() {
                Some((match self.state.usage.as_ref().and_then(|u| u.percent) {
                    None => usage.clone(),
                    Some(p) => format!("{p}% ctx"),
                }, usage.clone()))
            } else {
                None
            },
            cost: if details { Some(self.cost()).filter(|c| !c.is_empty()) } else { None },
            provider: if model_status_present(details, prompt, shell, &current) { model.as_ref().map(|m| m.0.clone()) } else { None },
            menu,
            spinner: if self.busy() { Some(if self.settings.mono { "*".into() } else { "\u{25aa}".into() }) } else { None },
        };
        footer_statusline_policy(&input).groups.into_iter().filter(|g| g.id != GroupId::Spinner).collect()
    }

    /// The status text for "Show status" (`footerStatus()`).
    fn footer_status(&self) -> String {
        let current = self.current_model.as_ref().map(|m| model_info(self.providers.as_deref(), m).1).unwrap_or_else(|| self.state.model.trim().to_string());
        let mut details = vec![if self.busy() { "running".to_string() } else { "idle".into() }, format!("agent {}", self.current_agent())];
        if !current.is_empty() {
            details.push(match &self.current_variant {
                Some(v) => format!("{current} {v}"),
                None => current,
            });
        }
        let usage = self.context_usage();
        if !usage.is_empty() {
            details.push(usage);
        }
        let cost = self.cost();
        if !cost.is_empty() {
            details.push(cost);
        }
        if !self.queued.is_empty() {
            details.push(format!("{} pending", self.queued.len()));
        }
        let active = self.active_tabs().len();
        if active > 0 {
            details.push(format!("{active} subagent{}", if active == 1 { "" } else { "s" }));
        }
        details.join(if self.settings.mono { " - " } else { " · " })
    }

    // --- panels --------------------------------------------------------------------------------

    fn panel_data(&self) -> PanelData {
        PanelData {
            subagents: self.subagent.tabs.clone(),
            queued: self.queued.clone(),
            variants: self.variants.clone(),
            variant_cycle: mono_shortcut(&self.keys.shortcut_all("variant.cycle"), self.settings.mono),
            clear_shortcut: self.shortcut("app.clear"),
            agents: self.agents.clone(),
            current_agent: self.current_agent_id(),
            providers: self.providers.clone(),
            current_model: self.current_model.clone(),
            current_variant: self.current_variant.clone(),
            settings: self.settings.clone(),
            current_subagent: match &self.route {
                PromptRoute::Subagent(id) => Some(id.clone()),
                _ => None,
            },
            undo_shortcut: self.shortcut("queued_prompt.undo"),
            delete_shortcut: self.shortcut("queued_prompt.delete"),
        }
    }

    fn panel_select(&mut self, action: PanelAction) {
        match action {
            PanelAction::Agent => self.open_route(PromptRoute::Agent),
            PanelAction::Model => self.open_route(PromptRoute::Model),
            PanelAction::Editor => {
                self.close_panel();
                self.with_composer(|c, f| c.open_editor(f, None));
            }
            PanelAction::Subagent => {
                if !self.subagent.tabs.is_empty() {
                    self.open_route(PromptRoute::SubagentMenu);
                }
            }
            PanelAction::Queued => {
                if !self.queued.is_empty() {
                    self.open_route(PromptRoute::QueuedMenu);
                }
            }
            PanelAction::VariantList => self.open_route(PromptRoute::Variant),
            PanelAction::Settings => self.open_route(PromptRoute::Settings),
            PanelAction::VariantCycle => {
                self.actions.push(FooterAction::CycleVariant);
                self.close_panel();
            }
            PanelAction::Status => {
                let s = self.footer_status();
                self.set_notice(s);
                self.close_panel();
            }
            PanelAction::Slash(name) => {
                let w = self.term_width;
                self.with_composer(|c, f| c.submit_text(f, w, &format!("/{name}")));
                self.close_panel();
            }
            PanelAction::Clear => {
                self.close_panel();
                self.actions.push(FooterAction::ClearScreen);
            }
            PanelAction::Exit => self.close(),
            PanelAction::PickAgent(a) => {
                self.handle_agent_select(a);
                self.close_panel();
            }
            PanelAction::PickModel(m) => {
                self.handle_model_select(m);
                self.close_panel();
            }
            PanelAction::PickVariant(v) => {
                if !self.is_closed() {
                    self.actions.push(FooterAction::VariantSelect(v));
                }
                self.close_panel();
            }
            PanelAction::PickSubagent(id) => self.open_tab(id),
            PanelAction::PickQueued(q) => {
                let action = if q.delivery == Delivery::Queue { QueuedPromptAction::Steer } else { QueuedPromptAction::Queue };
                self.actions.push(FooterAction::QueuedAction(action, q.message_id));
                self.close_panel();
            }
            PanelAction::Setting(_) => {}
        }
    }

    fn with_composer(&mut self, f: impl FnOnce(&mut Composer, &mut Footer)) {
        let mut c = std::mem::take(&mut self.composer);
        f(&mut c, self);
        self.composer = c;
    }

    // --- keys ----------------------------------------------------------------------------------

    fn matches(&self, id: &str, s: &Stroke) -> Match {
        let mut seq = self.pending.clone();
        seq.push(s.clone());
        self.keys.match_command(id, &seq)
    }

    /// Handle a key stroke.
    pub fn key(&mut self, s: Stroke, now: Instant) {
        self.now = now;
        if self.is_closed() {
            return;
        }
        // Leader / multi-stroke sequences: collect while any command has a prefix match.
        let ids = self.active_commands();
        let mut prefix = false;
        let mut full: Vec<String> = Vec::new();
        for id in &ids {
            match self.matches(id, &s) {
                Match::Full => full.push(id.clone()),
                Match::Prefix => prefix = true,
                Match::None => {}
            }
        }
        if full.is_empty() && prefix {
            self.pending.push(s);
            self.pending_at = Some(now);
            return;
        }
        let had_pending = !self.pending.is_empty();
        self.pending.clear();
        self.pending_at = None;
        for id in &full {
            if self.run_command(id) {
                self.after_input();
                return;
            }
        }
        if had_pending {
            return;
        }
        self.unbound_key(&s);
        self.after_input();
    }

    /// Commands of the enabled keymap layers, highest priority first.
    fn active_commands(&self) -> Vec<String> {
        let mut v: Vec<&str> = Vec::new();
        let prompt = self.prompt_active();
        let visible = self.composer.visible();
        let composer_route = matches!(self.view, FooterView::Prompt) && self.route == PromptRoute::Composer;
        // Priority 1 layers.
        if self.route == PromptRoute::QueuedMenu && matches!(self.view, FooterView::Prompt) {
            v.extend(["queued_prompt.undo", "queued_prompt.delete"]);
        }
        if self.inspecting() && self.selected_tab().is_some_and(|t| t.status == SubagentStatus::Running) {
            v.push("composer.subagent.interrupt");
        }
        if composer_route && self.active_tabs().iter().any(|t| !t.background) {
            v.push("session.background");
        }
        if prompt && (!visible || self.composer.mode == super::prompt::MenuMode::Slash) {
            v.push("prompt.queue");
        }
        if prompt && !visible {
            v.extend(["prompt.editor", "prompt.history.previous", "prompt.history.next"]);
        }
        // Priority 0 layers, latest registered first.
        if prompt && visible {
            v.extend(["prompt.autocomplete.prev", "prompt.autocomplete.next", "prompt.autocomplete.hide", "prompt.autocomplete.select", "prompt.autocomplete.complete"]);
        }
        if prompt {
            v.extend(["prompt.paste", "session.interrupt"]);
        }
        if self.base_bindings_enabled() {
            v.push("prompt.clear");
        }
        if composer_route && !self.queued.is_empty() {
            v.push("session.queued_prompts");
        }
        if composer_route && !self.subagent.tabs.is_empty() {
            v.push("session.child.first");
        }
        if composer_route && !visible {
            v.extend(["command.palette.show", "variant.cycle"]);
        }
        v.push("app.clear");
        if prompt {
            v.extend([
                "input.submit", "input.newline", "input.move.left", "input.move.right", "input.move.up", "input.move.down", "input.line.home", "input.line.end", "input.visual.line.home", "input.visual.line.end", "input.buffer.home", "input.buffer.end", "input.delete.line", "input.delete.to.line.end", "input.delete.to.line.start", "input.backspace", "input.delete", "input.undo", "input.redo", "input.word.forward", "input.word.backward", "input.delete.word.forward", "input.delete.word.backward",
            ]);
        }
        v.into_iter().map(String::from).collect()
    }

    fn selected_tab(&self) -> Option<&SubagentTab> {
        match &self.route {
            PromptRoute::Subagent(id) => self.subagent.tabs.iter().find(|t| &t.session_id == id),
            _ => None,
        }
    }

    /// Run a command; false lets the key fall through to the next binding.
    fn run_command(&mut self, id: &str) -> bool {
        let w = self.term_width;
        match id {
            "app.clear" => {
                self.actions.push(FooterAction::ClearScreen);
                true
            }
            "command.palette.show" => {
                self.open_route(PromptRoute::Command);
                true
            }
            "variant.cycle" => {
                self.actions.push(FooterAction::CycleVariant);
                true
            }
            "session.background" => {
                self.actions.push(FooterAction::Background);
                true
            }
            "session.child.first" => {
                if !self.subagent.tabs.is_empty() {
                    self.open_route(PromptRoute::SubagentMenu);
                }
                true
            }
            "session.queued_prompts" => {
                if !self.queued.is_empty() {
                    self.open_route(PromptRoute::QueuedMenu);
                }
                true
            }
            "composer.subagent.interrupt" => {
                if let Some(t) = self.selected_tab()
                    && t.status == SubagentStatus::Running
                {
                    let id = t.session_id.clone();
                    self.actions.push(FooterAction::SubagentInterrupt(id));
                }
                true
            }
            "queued_prompt.undo" | "queued_prompt.delete" => {
                let d = self.panel_data();
                let Some(item) = self.panel.as_ref().and_then(|p| p.items(&d).get(p.menu.selected as usize).cloned()) else { return false };
                let PanelAction::PickQueued(q) = item.action else { return false };
                if id == "queued_prompt.delete" {
                    self.actions.push(FooterAction::QueuedAction(QueuedPromptAction::Cancel, q.message_id));
                } else {
                    let current = self.composer.current();
                    if current.shell && !current.text.is_empty() {
                        self.set_notice("leave shell mode before undoing a queued prompt".into());
                        return true;
                    }
                    self.actions.push(FooterAction::QueuedAction(QueuedPromptAction::Cancel, q.message_id.clone()));
                    self.close_panel();
                    let merged = super::prompt_shared::prompt_append(&current, &q.prompt);
                    self.composer.replace_prompt(&merged);
                }
                true
            }
            "prompt.clear" => {
                let active = self.prompt_active();
                let mut c = std::mem::take(&mut self.composer);
                let r = c.request_exit(self, active);
                self.composer = c;
                r
            }
            "prompt.paste" => {
                self.actions.push(FooterAction::Paste);
                true
            }
            "session.interrupt" => self.handle_interrupt(),
            "prompt.queue" => {
                self.with_composer(|c, f| c.on_submit(f, w, Delivery::Queue));
                true
            }
            "prompt.editor" => {
                self.with_composer(|c, f| c.open_editor(f, None));
                true
            }
            "prompt.history.previous" => self.composer.history_command(-1),
            "prompt.history.next" => self.composer.history_command(1),
            "prompt.autocomplete.prev" => {
                let mut c = std::mem::take(&mut self.composer);
                c.menu_move(self, w, -1);
                self.composer = c;
                true
            }
            "prompt.autocomplete.next" => {
                let mut c = std::mem::take(&mut self.composer);
                c.menu_move(self, w, 1);
                self.composer = c;
                true
            }
            "prompt.autocomplete.hide" => {
                self.composer.autocomplete_hide();
                true
            }
            "prompt.autocomplete.select" => {
                self.with_composer(|c, f| c.autocomplete_select(f, w, false));
                true
            }
            "prompt.autocomplete.complete" => {
                self.with_composer(|c, f| c.autocomplete_select(f, w, true));
                true
            }
            "input.submit" => {
                self.with_composer(|c, f| c.on_submit(f, w, Delivery::Steer));
                true
            }
            other if other.starts_with("input.") => {
                self.input_command(other);
                true
            }
            _ => false,
        }
    }

    fn input_command(&mut self, id: &str) {
        let width = self.composer.width;
        let a = &mut self.composer.area;
        let before = a.plain_text().to_string();
        match id {
            "input.newline" => a.newline(),
            "input.move.left" => a.move_left(),
            "input.move.right" => a.move_right(),
            "input.move.up" => {
                a.move_up(width);
            }
            "input.move.down" => {
                a.move_down(width);
            }
            "input.line.home" => a.line_home(),
            "input.line.end" => a.line_end(),
            "input.visual.line.home" => a.visual_home(width),
            "input.visual.line.end" => a.visual_end(width),
            "input.buffer.home" => a.buffer_home(),
            "input.buffer.end" => a.buffer_end(),
            "input.delete.line" => a.delete_line(),
            "input.delete.to.line.end" => a.delete_to_line_end(),
            "input.delete.to.line.start" => a.delete_to_line_start(),
            "input.backspace" => {
                if self.composer.shell && a.cursor_offset() == 0 {
                    self.composer.leave_shell(true);
                    return;
                }
                a.delete_char_backward();
            }
            "input.delete" => a.delete_char(),
            "input.undo" => a.undo(),
            "input.redo" => a.redo(),
            "input.word.forward" => a.word_forward(),
            "input.word.backward" => a.word_backward(),
            "input.delete.word.forward" => a.delete_word_forward(),
            "input.delete.word.backward" => a.delete_word_backward(),
            _ => {}
        }
        if self.composer.area.plain_text() != before {
            self.with_composer(|c, f| c.content_changed(f));
        } else {
            // Cursor moves can close the mention menu.
            self.with_composer(|c, f| {
                if c.visible() {
                    c.refresh(f);
                }
            });
        }
    }

    /// Keys no command claimed: blocker bodies, panels, the inspector, or text input.
    fn unbound_key(&mut self, s: &Stroke) {
        match &self.view {
            FooterView::Permission(p) => {
                let p = p.clone();
                let directory = self.directory.clone();
                if let Some(body) = self.permission.as_mut()
                    && let Some(reply) = body.key(s, &p, Some(&directory), self.settings.mono, self.term_width, self.height)
                {
                    self.actions.push(FooterAction::PermissionReply(reply));
                }
                return;
            }
            FooterView::Form(f) => {
                let f = f.clone();
                if let Some(body) = self.form.as_mut() {
                    match body.key(s, &f, self.term_width) {
                        FormOutcome::Reply(r) => self.actions.push(FooterAction::FormReply(r)),
                        FormOutcome::Cancel => self.actions.push(FooterAction::FormCancel { session_id: f.form.session_id.clone(), form_id: f.form.id.clone(), location: f.location.clone() }),
                        FormOutcome::None => {}
                    }
                }
                return;
            }
            FooterView::Prompt => {}
        }
        if self.panel.is_some() {
            let d = self.panel_data();
            let th = self.term_height;
            let mut panel = self.panel.take().unwrap();
            let out = panel.key(&d, th, s);
            self.panel = Some(panel);
            match out {
                PanelOutcome::None => {}
                PanelOutcome::Close => self.close_panel(),
                PanelOutcome::Select(a) => self.panel_select(a),
                PanelOutcome::Change(PanelAction::Setting(key), dir) => {
                    if self.panel.as_ref().is_some_and(|p| p.saving.is_some()) {
                        return;
                    }
                    if let Some(change) = panels::setting_change(key, &self.settings, dir) {
                        if let Some(p) = self.panel.as_mut() {
                            p.saving = Some(key);
                        }
                        self.actions.push(FooterAction::MiniSetting(change));
                    }
                }
                PanelOutcome::Change(..) => {}
                PanelOutcome::Undo(_) | PanelOutcome::Delete(_) => {}
            }
            if self.route == PromptRoute::SubagentMenu || self.route == PromptRoute::QueuedMenu {
                let d = self.panel_data();
                if let Some(p) = self.panel.as_ref() {
                    self.subagent_menu_rows = p.reported_rows(&d, self.term_height);
                }
                self.apply_height();
            }
            return;
        }
        if self.inspecting() {
            let name = s.name.as_str();
            if name == "escape" {
                self.open_route(PromptRoute::Composer);
            } else if name == "tab" && !s.shift {
                self.cycle_tab(1);
            } else {
                self.inspector.key(s);
            }
            return;
        }
        if !self.prompt_active() {
            return;
        }
        // Shell mode keys and `!`.
        let visible = self.composer.visible();
        if !visible {
            if s.is_char() == Some('!') && self.composer.enter_shell() {
                return;
            }
            if self.composer.shell && s.name == "escape" {
                self.composer.leave_shell(false);
                return;
            }
        }
        if s.name == "escape" && self.state.phase == FooterPhase::Idle {
            self.handle_input_clear();
            return;
        }
        if let Some(c) = s.is_char() {
            self.composer.area.insert_text(&c.to_string());
            self.with_composer(|c, f| c.content_changed(f));
        }
    }

    fn after_input(&mut self) {
        if !self.destroyed {
            self.sync_routes();
            self.apply_height();
        }
    }

    /// Bracketed paste.
    pub fn paste(&mut self, text: &str) {
        if self.is_closed() {
            return;
        }
        if let Some(p) = self.panel.as_mut() {
            p.paste(text);
            return;
        }
        if let FooterView::Permission(_) = &self.view {
            if let Some(b) = self.permission.as_mut() {
                b.paste(text);
            }
            return;
        }
        if let FooterView::Form(_) = &self.view {
            if let Some(b) = self.form.as_mut() {
                b.paste(text);
            }
            return;
        }
        if !self.prompt_active() {
            return;
        }
        self.with_composer(|c, f| c.paste(f, text));
        self.with_composer(|c, f| c.content_changed(f));
        self.apply_height();
    }

    /// The editor returned.
    pub fn editor_result(&mut self, content: Result<Option<String>, ()>) {
        self.with_composer(|c, f| c.editor_result(f, content));
        self.apply_height();
    }

    pub fn files_found(&mut self, query: &str, files: Vec<String>) {
        self.composer.set_files(query, files);
        self.apply_height();
    }

    // --- render --------------------------------------------------------------------------------

    /// Render the footer frame at the current height.
    pub fn render(&mut self, now: Instant) -> (Buffer, Option<CursorReq>) {
        self.now = now;
        let width = self.term_width.max(1);
        let height = self.height.max(1);
        let mut buf = Buffer::new(width as u16, height as u16);
        let bg = self.theme.background;
        for c in buf.cells.iter_mut() {
            c.glyph = Glyph::Text(" ".into());
            c.bg = bg;
            c.fg = Color::WHITE;
        }
        let mut y = 0usize;
        let mut cursor: Option<(usize, usize)> = None;
        // Startup row.
        if let Some((version, detail)) = self.startup.clone() {
            let l = splash::entry_splash_layout(width, &version, Some(&detail), self.settings.mono);
            let mut x = 0;
            let label = if let Some(rest) = l.label.strip_prefix('\u{25aa}') {
                let m = motion::seed_launch();
                let age = now.duration_since(self.startup_at).as_secs_f64() * 1000.0;
                let (glyph, level) = if self.animations { let f = motion::one_cell_frame(&m, age); (f.glyph, f.level) } else { ("\u{25aa}".into(), 1.0) };
                let c = self.theme.splash.right;
                let color = Color { a: (c.a as f64 * level).round() as u8, ..c };
                buf.set_blend(0, 0, Glyph::Text(glyph), color, Color::TRANSPARENT, 0);
                x = 1;
                rest.to_string()
            } else {
                l.label.clone()
            };
            buf.draw_chunks(x, 0, &[StyledChunk::plain(label, self.theme.splash.right), StyledChunk::plain(l.metadata, self.theme.splash.left)], width.saturating_sub(x as usize));
            y += 1;
        }
        if self.panel_open() || self.inspecting() {
            y += 1;
        }
        let theme = self.theme.footer.clone();
        let mono = self.settings.mono;
        if self.inspecting() {
            let rows = height.saturating_sub(y);
            let tab = self.selected_tab().cloned();
            let index = match &self.route {
                PromptRoute::Subagent(id) => self.subagent.tabs.iter().position(|t| &t.session_id == id).map(|i| i + 1).unwrap_or(0),
                _ => 0,
            };
            let detail = tab.as_ref().and_then(|t| self.subagent.details.get(&t.session_id)).cloned().unwrap_or_default();
            let interrupt = Some(self.shortcut("composer.subagent.interrupt")).filter(|s| !s.is_empty());
            self.inspector.render(&mut buf, y, width, rows, &self.theme, tab.as_ref(), index, self.subagent.tabs.len(), &detail, interrupt.as_deref(), self.settings.shell_output, mono);
            return (buf, None);
        }
        match self.view.clone() {
            FooterView::Permission(p) => {
                let rows = height.saturating_sub(y);
                let directory = self.directory.clone();
                if let Some(b) = self.permission.as_mut() {
                    cursor = b.render(&mut buf, y, width, rows, &theme, &self.theme.block, &p, Some(&directory), mono);
                }
            }
            FooterView::Form(f) => {
                let rows = height.saturating_sub(y);
                if let Some(b) = self.form.as_mut() {
                    cursor = b.render(&mut buf, y, width, rows, &theme, &f, mono);
                }
            }
            FooterView::Prompt => {
                if self.panel.is_some() {
                    let d = self.panel_data();
                    let th = self.term_height;
                    let mut panel = self.panel.take().unwrap();
                    panel.sync(&d, th);
                    cursor = Some(panel.render(&mut buf, y, width, th, &theme, &d, mono));
                    self.panel = Some(panel);
                } else if self.route == PromptRoute::Composer {
                    let l = self.prompt_layout();
                    let rail = if self.composer.shell { theme.formfield_focused_text } else { self.agent_color() };
                    let placeholder = self.composer.placeholder(self.state.first, mono);
                    let (cx, cy) = self.composer.render(&mut buf, y, width, &l, rail, theme.text, theme.muted, mono, &placeholder);
                    cursor = Some((cx, cy));
                    let text_rows = self.composer.shown_rows(&l);
                    y += l.padding * 2 + text_rows + l.images;
                    if self.composer.visible() {
                        let options = self.composer.options(self, self.term_width);
                        let items: Vec<_> = options.iter().map(|o| o.item.clone()).collect();
                        let limit = l.menu.max(1);
                        self.composer.menu.reconcile(items.len() as i64, limit as i64);
                        let rows = self.composer.menu.rows(items.len(), limit);
                        let mut view = MenuView::new(&items, self.composer.menu, rows);
                        view.limit = FOOTER_MENU_ROWS;
                        view.border = false;
                        view.padding_left = 2;
                        view.padding_right = 2;
                        view.mono = mono;
                        view.render(&mut buf, y, width, &theme);
                    } else {
                        self.render_statusline(&mut buf, y, width, now);
                    }
                }
            }
        }
        let cursor = cursor.filter(|_| !self.is_closed()).map(|(x, y)| CursorReq { x: x as u16, y: y as u16, style: self.cursor_style, color: None });
        (buf, cursor)
    }

    fn render_statusline(&self, buf: &mut Buffer, y: usize, width: usize, now: Instant) {
        let sections = self.status_sections();
        let mut x = 0usize;
        if self.busy() {
            let motion = if self.settings.mono { motion::seed_mono() } else { motion::work_spinner(&self.settings.work_spinner) };
            let age = self.busy_since.map(|t| now.duration_since(t).as_secs_f64() * 1000.0).unwrap_or(0.0);
            let (glyph, level) = if self.animations { motion::spinner_frame(&motion, age, !self.settings.mono) } else { (if self.settings.mono { "*".into() } else { "\u{25aa}".into() }, 1.0) };
            let c = self.agent_color();
            let color = Color { a: (c.a as f64 * level).round() as u8, ..c };
            buf.set_blend(0, y as i32, Glyph::Text(glyph), color, Color::TRANSPARENT, 0);
            x = 2;
        }
        if sections.is_empty() {
            return;
        }
        let t = &self.theme.footer;
        let status_color = self.status_color();
        let agent = self.agent_color();
        let mut chunks = Vec::new();
        for (i, s) in sections.iter().enumerate() {
            if i > 0 {
                chunks.push(StyledChunk::plain(if self.settings.mono { " - " } else { " · " }, t.muted));
            }
            for (text, tone) in &s.parts {
                let fg = match tone {
                    Tone::Text => t.text,
                    Tone::Muted => t.muted,
                    Tone::Agent => agent,
                    Tone::Status => status_color,
                };
                chunks.push(StyledChunk::plain(text.clone(), fg));
            }
        }
        let w = self.status_width();
        let rows = self.status_rows();
        for (i, l) in layout(&chunks, w, WrapMode::Word, false).iter().take(rows).enumerate() {
            buf.draw_chunks(x as i32, (y + i) as i32, &l.chunks, width.saturating_sub(x));
        }
    }
}

fn model_status_present(details: bool, prompt: bool, shell: bool, current: &str) -> bool {
    details && prompt && !shell && !current.is_empty()
}

impl Host for Footer {
    fn agents(&self) -> &[RunAgent] {
        &self.agents
    }
    fn references(&self) -> &[RunReference] {
        &self.references
    }
    fn commands(&self) -> Option<&[RunCommand]> {
        self.commands.as_deref()
    }
    fn state(&self) -> &FooterState {
        &self.state
    }
    fn queued_prompts(&self) -> Vec<QueuedPrompt> {
        self.queued.clone()
    }
    fn directory(&self) -> String {
        self.directory.clone()
    }
    fn term_height(&self) -> usize {
        self.term_height
    }
    fn mono(&self) -> bool {
        self.settings.mono
    }
    fn submit(&mut self, prompt: RunPrompt) -> bool {
        self.handle_prompt(prompt)
    }
    fn cycle(&mut self) {
        self.actions.push(FooterAction::CycleVariant);
    }
    fn interrupt(&mut self) -> bool {
        self.handle_interrupt()
    }
    fn editor_open(&mut self, value: String) {
        self.finish_startup();
        self.actions.push(FooterAction::EditorOpen(value));
    }
    fn input_clear(&mut self) {
        self.handle_input_clear();
    }
    fn exit_request(&mut self) -> bool {
        self.handle_exit()
    }
    fn exit(&mut self) {
        self.close();
    }
    fn settings(&mut self) {
        self.open_route(PromptRoute::Settings);
    }
    fn status(&mut self, text: String) {
        self.set_notice(text);
    }
    fn queued_steer(&mut self, inbox_id: String) {
        self.actions.push(FooterAction::QueuedAction(QueuedPromptAction::Steer, inbox_id));
    }
    fn find_files(&mut self, query: String) {
        self.actions.push(FooterAction::FindFiles(query));
    }
    fn paste_clipboard(&mut self) {
        self.actions.push(FooterAction::Paste);
    }
}

#[cfg(test)]
#[path = "footer_golden.rs"]
mod golden;
