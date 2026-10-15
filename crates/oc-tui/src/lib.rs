//! Terminal UI client for the opencode HTTP API (port of opencode v2 `packages/tui`).
//!
//! Entry point: [`run`] with [`TuiOptions`]. The TUI talks to a server only over HTTP
//! (`/api/*`) and the SSE event stream (`GET /api/event`).

pub mod api;
pub mod app;
pub mod attention;
pub mod clipboard;
pub mod config;
pub mod keymap;
pub mod mini;
mod restore;
pub mod state;
pub mod store;
pub mod terminal;
pub mod theme;
pub mod ui;
pub mod util;

pub use app::{App, AppMsg, Route, TuiOptions};
pub use mini::{MiniOptions, run_mini};

use crossterm::event::{
    DisableBracketedPaste, DisableFocusChange, DisableMouseCapture, EnableBracketedPaste, EnableFocusChange, EnableMouseCapture, EventStream, KeyboardEnhancementFlags,
    PopKeyboardEnhancementFlags, PushKeyboardEnhancementFlags,
};
use crossterm::execute;
use crossterm::terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode};
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use std::io::Write;

type Term = Terminal<CrosstermBackend<std::io::Stdout>>;

struct Screen {
    kitty: bool,
    mouse: bool,
}

impl Screen {
    fn enter(&self) -> anyhow::Result<()> {
        let mut modes = restore::ALT_SCREEN | restore::FOCUS | restore::PASTE;
        if self.mouse {
            modes |= restore::MOUSE;
        }
        if self.kitty {
            modes |= restore::KITTY;
        }
        restore::arm(modes);
        enable_raw_mode()?;
        let mut out = std::io::stdout();
        execute!(out, EnterAlternateScreen, EnableBracketedPaste, EnableFocusChange)?;
        if self.mouse {
            execute!(out, EnableMouseCapture)?;
        }
        if self.kitty {
            execute!(out, PushKeyboardEnhancementFlags(KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES | KeyboardEnhancementFlags::REPORT_ALTERNATE_KEYS))?;
        }
        Ok(())
    }

    fn leave(&self) {
        let mut out = std::io::stdout();
        if self.kitty {
            let _ = execute!(out, PopKeyboardEnhancementFlags);
        }
        let _ = execute!(out, DisableMouseCapture, DisableFocusChange, DisableBracketedPaste, LeaveAlternateScreen);
        let _ = write!(out, "\x1b]0;\x07");
        let _ = out.flush();
        let _ = disable_raw_mode();
        restore::disarm();
    }
}

impl Drop for Screen {
    /// An early `?` between `enter` and `leave` still gives the terminal back.
    fn drop(&mut self) {
        if restore::armed() {
            self.leave();
        }
    }
}

fn set_title(app: &App, last: &mut String) {
    if !app.config.terminal_title() {
        return;
    }
    let title = match (&app.route, app.session()) {
        (Route::Plugin(name), _) => format!("OC | {name}"),
        (_, s) => match s {
        Some(s) if s.title.as_deref().is_some_and(|t| !t.is_empty() && !crate::util::locale::is_default_title(t)) => {
            let t = s.title.clone().unwrap();
            if t.chars().count() > 40 { format!("OC | {}…", t.chars().take(37).collect::<String>()) } else { format!("OC | {t}") }
        }
        _ => "OpenCode".to_string(),
        },
    };
    if *last != title {
        let _ = write!(std::io::stdout(), "\x1b]0;{title}\x07");
        *last = title;
    }
}

/// Run the external editor on the prompt text (`src/editor.ts`).
fn external_editor(app: &mut App, screen: &Screen, term: &mut Term) {
    app.editor_requested = false;
    let Some(cmd) = std::env::var("VISUAL").ok().filter(|v| !v.is_empty()).or_else(|| std::env::var("EDITOR").ok().filter(|v| !v.is_empty())) else {
        return;
    };
    let path = std::env::temp_dir().join(format!("{}.md", crate::util::locale::now_ms()));
    if std::fs::write(&path, app.editor.input_text()).is_err() {
        return;
    }
    screen.leave();
    let mut parts = cmd.split(' ').filter(|s| !s.is_empty());
    let program = parts.next().unwrap_or_default().to_string();
    let args: Vec<String> = parts.map(|s| s.to_string()).collect();
    let status = std::process::Command::new(&program).args(&args).arg(&path).current_dir(app.location_dir()).status();
    let _ = screen.enter();
    let _ = term.clear();
    match status {
        Ok(s) if s.success() => {
            if let Ok(text) = std::fs::read_to_string(&path)
                && !text.is_empty()
            {
                let text = if !text.trim_end_matches(['\n', '\r']).contains('\n') { text.strip_suffix("\r\n").or_else(|| text.strip_suffix('\n')).unwrap_or(&text).to_string() } else { text };
                let mut info = crate::state::PromptInfo::empty();
                info.text = text;
                app.editor.set(&info);
            }
        }
        Ok(s) => {
            let msg = match s.code() {
                Some(c) => format!("Editor exited with code {c}"),
                None => "Editor exited with signal".to_string(),
            };
            app.toast(app::Toast::new("error", msg));
        }
        Err(e) => app.toast(app::Toast::new("error", e.to_string())),
    }
    let _ = std::fs::remove_file(&path);
    app.dirty = true;
}

#[cfg(unix)]
fn suspend(app: &mut App, screen: &Screen, term: &mut Term) {
    app.suspend_requested = false;
    screen.leave();
    unsafe {
        libc::raise(libc::SIGTSTP);
    }
    let _ = screen.enter();
    let _ = term.clear();
    app.dirty = true;
}

#[cfg(not(unix))]
fn suspend(app: &mut App, _: &Screen, _: &mut Term) {
    app.suspend_requested = false;
}

/// The full-screen terminal's steps of the event loop.
struct TermHooks<'a> {
    screen: &'a Screen,
    term: &'a mut Term,
    title: String,
}

impl app::event_loop::Hooks for TermHooks<'_> {
    fn draw(&mut self, app: &mut App) -> anyhow::Result<()> {
        self.term.draw(|f| app.draw(f))?;
        set_title(app, &mut self.title);
        Ok(())
    }
    fn editor(&mut self, app: &mut App) -> anyhow::Result<()> {
        external_editor(app, self.screen, self.term);
        Ok(())
    }
    fn suspend(&mut self, app: &mut App) -> anyhow::Result<()> {
        suspend(app, self.screen, self.term);
        Ok(())
    }
}

/// Run the TUI until the user exits. Returns the process exit code.
pub async fn run(opts: TuiOptions) -> anyhow::Result<i32> {
    let config = config::Config::load();
    let kitty = crossterm::terminal::supports_keyboard_enhancement().unwrap_or(false);
    let screen = Screen { kitty, mouse: config.mouse() };
    screen.enter()?;
    // Theme mode + palette probe before the input reader starts.
    let palette = terminal::probe(1000);
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<AppMsg>();
    let mut app = App::new(opts, config, tx.clone());
    if let Ok(custom) = theme::discover(std::path::Path::new(&app.opts.directory)) {
        app.themes.set_custom(custom);
    }
    if let Some(colors) = &palette {
        if app.themes.lock.is_none()
            && let Some(m) = theme::terminal_mode(colors)
        {
            app.themes.set_mode(m);
        }
        app.themes.set_system_palette(Some(colors.clone()));
        app.keyed.update("system-theme", |v| *v = serde_json::json!({ "colors": colors }));
    }
    app.reload_theme();

    let backend = CrosstermBackend::new(std::io::stdout());
    let mut term = Terminal::new(backend)?;
    term.clear()?;

    // Event stream.
    let client = app.client.clone();
    let sse_tx = tx.clone();
    let (stx, mut srx) = tokio::sync::mpsc::unbounded_channel();
    tokio::spawn(api::sse::run(client, stx));
    tokio::spawn(async move {
        while let Some(m) = srx.recv().await {
            if sse_tx.send(AppMsg::Sse(m)).is_err() {
                break;
            }
        }
    });

    app.start_migration_poll();

    let mut events = app::escape::JoinEscapes::new(EventStream::new());
    let mut hooks = TermHooks { screen: &screen, term: &mut term, title: String::new() };
    let result = app::event_loop::run(&mut app, &mut events, &mut rx, &mut hooks).await;
    screen.leave();
    if let Err(e) = result {
        eprintln!("{e}");
        return Ok(1);
    }
    if let Some(e) = &app.epilogue {
        println!("{e}");
    }
    Ok(0)
}
