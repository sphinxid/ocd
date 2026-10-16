//! `opencode mini` runner (`cli/src/mini.ts`, `src/mini/`): no alternate screen; the transcript
//! goes to native scrollback and a live footer is redrawn below it.

use crate::app::mini::{MiniState, lines_to_ansi, row_to_ansi};
use crate::app::{App, AppMsg, TuiOptions};
use crate::keymap::{self, Match};
use crossterm::event::{
    DisableBracketedPaste, EnableBracketedPaste, Event as CEvent, EventStream, KeyboardEnhancementFlags, PopKeyboardEnhancementFlags, PushKeyboardEnhancementFlags,
};
use crossterm::execute;
use crossterm::terminal::{disable_raw_mode, enable_raw_mode};
use std::io::{IsTerminal, Read, Write};

/// Mini-only flags (`opencode mini --replay/--no-replay --replay-limit`).
#[derive(Clone, Debug, Default)]
pub struct MiniOptions {
    /// `--replay` / `--no-replay`; `None` falls back to `mini.replay`, then `true`.
    pub replay: Option<bool>,
    /// `--replay-limit`; `None` falls back to `mini.replay_limit`, then 200.
    pub replay_limit: Option<i64>,
}

fn fail(msg: &str) -> i32 {
    eprintln!("\x1b[91m\x1b[1mError: \x1b[0m{msg}");
    1
}

/// `mergeInput(piped, prompt)`: `piped + "\n" + prompt` when both are present.
pub fn merge_input(piped: Option<&str>, prompt: Option<&str>) -> Option<String> {
    let piped = piped.map(|p| p.trim_end_matches(['\n', '\r'])).filter(|p| !p.trim().is_empty());
    let prompt = prompt.filter(|p| !p.trim().is_empty());
    match (piped, prompt) {
        (Some(a), Some(b)) => Some(format!("{a}\n{b}")),
        (Some(a), None) => Some(a.to_string()),
        (None, Some(b)) => Some(b.to_string()),
        (None, None) => None,
    }
}

/// Live footer painter: tracks where the footer starts so it can be erased and redrawn.
struct Footer {
    /// Row of the terminal cursor inside the footer (0 = first footer row).
    cursor_row: u16,
    drawn: bool,
}

impl Footer {
    fn erase(&mut self, out: &mut String) {
        if self.drawn {
            out.push('\r');
            if self.cursor_row > 0 {
                out.push_str(&format!("\x1b[{}A", self.cursor_row));
            }
            out.push_str("\x1b[J");
        }
        self.drawn = false;
        self.cursor_row = 0;
    }
}

fn frame(app: &mut App, st: &mut MiniState, footer: &mut Footer, extra: &[String]) {
    let (w, h) = crossterm::terminal::size().unwrap_or((80, 24));
    app.size = (w, h);
    let mut out = String::from("\x1b[?2026h\x1b[?25l");
    footer.erase(&mut out);
    for l in extra {
        out.push_str(l);
        out.push_str("\r\n");
    }
    let (commit, tail) = app.mini_transcript(st, w);
    for l in lines_to_ansi(&commit, w) {
        out.push_str(&l);
        out.push_str("\r\n");
    }
    let (buf, cursor) = app.mini_footer(w, h.saturating_sub(1).max(1), &tail);
    let fh = buf.area.height;
    for y in 0..fh {
        out.push_str("\x1b[2K");
        out.push_str(&row_to_ansi(&buf, y));
        if y + 1 < fh {
            out.push_str("\r\n");
        }
    }
    footer.drawn = true;
    footer.cursor_row = fh.saturating_sub(1);
    if let Some((cx, cy)) = cursor {
        let up = fh.saturating_sub(1).saturating_sub(cy);
        if up > 0 {
            out.push_str(&format!("\x1b[{up}A"));
        }
        out.push_str(&format!("\x1b[{}G\x1b[?25h", cx + 1));
        footer.cursor_row = cy;
    }
    out.push_str("\x1b[?2026l");
    let mut stdout = std::io::stdout();
    let _ = stdout.write_all(out.as_bytes());
    let _ = stdout.flush();
}

fn leave(kitty: bool) {
    let mut out = std::io::stdout();
    if kitty {
        let _ = execute!(out, PopKeyboardEnhancementFlags);
    }
    let _ = execute!(out, DisableBracketedPaste);
    let _ = write!(out, "\x1b[?25h");
    let _ = out.flush();
    let _ = disable_raw_mode();
    crate::restore::disarm();
}

fn enter(kitty: bool) -> anyhow::Result<()> {
    crate::restore::arm(crate::restore::PASTE | if kitty { crate::restore::KITTY } else { 0 });
    enable_raw_mode()?;
    let mut out = std::io::stdout();
    execute!(out, EnableBracketedPaste)?;
    if kitty {
        execute!(out, PushKeyboardEnhancementFlags(KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES | KeyboardEnhancementFlags::REPORT_ALTERNATE_KEYS))?;
    }
    Ok(())
}

/// Mini mode's steps of the event loop: footer painting, scrollback commits, clear/re-wrap.
struct MiniHooks {
    st: MiniState,
    footer: Footer,
    /// Rows waiting to be printed above the footer (header, replays).
    pending: Vec<String>,
    splash: bool,
    kitty: bool,
    last_width: u16,
}

impl MiniHooks {
    /// Leave the terminal for the editor or a suspend, then come back.
    fn away(&mut self, app: &mut App) -> anyhow::Result<()> {
        let mut out = String::new();
        self.footer.erase(&mut out);
        print!("{out}");
        let _ = std::io::stdout().flush();
        leave(self.kitty);
        if app.suspend_requested {
            app.suspend_requested = false;
            #[cfg(unix)]
            unsafe {
                libc::raise(libc::SIGTSTP);
            }
        } else {
            app.editor_requested = false;
            run_editor(app);
        }
        enter(self.kitty)?;
        app.dirty = true;
        Ok(())
    }

    /// Clear the screen and replay the session; the header is printed again with `splash`.
    fn replay(&mut self, app: &App, width: u16) {
        self.footer.drawn = false;
        print!("\x1b[2J\x1b[3J\x1b[H");
        self.st.session = None;
        self.st.resumed = true;
        if self.splash {
            self.pending.extend(lines_to_ansi(&[app.mini_header(width)], width));
        }
    }
}

impl crate::app::event_loop::Hooks for MiniHooks {
    fn draw(&mut self, app: &mut App) -> anyhow::Result<()> {
        frame(app, &mut self.st, &mut self.footer, &self.pending);
        self.pending.clear();
        Ok(())
    }
    fn wants_draw(&self) -> bool {
        !self.pending.is_empty()
    }
    fn editor(&mut self, app: &mut App) -> anyhow::Result<()> {
        self.away(app)
    }
    fn suspend(&mut self, app: &mut App) -> anyhow::Result<()> {
        self.away(app)
    }
    fn intercept(&mut self, app: &mut App, ev: &CEvent) -> bool {
        match ev {
            CEvent::Key(k) => {
                let Some(s) = keymap::stroke_from_event(k) else { return true };
                if app.dialogs.is_empty() && app.pending_keys.is_empty() && app.keybinds.match_command("app.clear", std::slice::from_ref(&s)) == Match::Full {
                    // Clear the screen and replay the session.
                    let w = crossterm::terminal::size().map(|s| s.0).unwrap_or(80);
                    self.replay(app, w);
                    app.dirty = true;
                    return true;
                }
                false
            }
            CEvent::Paste(_) => false,
            CEvent::Resize(w, _) => {
                if *w != self.last_width && self.st.replay {
                    // Re-wrap: clear and replay at the new width.
                    self.replay(app, *w);
                }
                self.last_width = *w;
                app.dirty = true;
                true
            }
            // Mini mode has no mouse capture or focus reporting.
            _ => true,
        }
    }
}

/// Run the minimal interactive interface until exit. Returns the process exit code.
pub async fn run_mini(mut opts: TuiOptions, mini: MiniOptions) -> anyhow::Result<i32> {
    if !std::io::stdout().is_terminal() {
        eprint!("\x1b[91m\x1b[1mError: \x1b[0mocd mini requires a TTY stdout\n");
        return Ok(1);
    }
    let config = crate::config::Config::load();
    let replay = mini.replay.or_else(|| config.bool("mini.replay")).unwrap_or(true);
    let limit = mini.replay_limit.or_else(|| config.num("mini.replay_limit").map(|n| n as i64));
    if limit.is_some_and(|n| n <= 0) {
        return Ok(fail("--replay-limit must be a positive integer"));
    }
    if opts.fork && opts.session.is_none() && !opts.continue_last {
        return Ok(fail("--fork requires --continue or --session"));
    }
    // Piped stdin becomes the initial input; keys then come from the controlling terminal.
    if !std::io::stdin().is_terminal() {
        let mut piped = String::new();
        let _ = std::io::stdin().read_to_string(&mut piped);
        opts.prompt = merge_input(Some(&piped), opts.prompt.as_deref());
        #[cfg(unix)]
        if std::fs::OpenOptions::new().read(true).write(true).open("/dev/tty").is_err() {
            return Ok(fail("ocd mini requires a controlling terminal for input"));
        }
    }
    let splash = config.str("mini.splash") != Some("hide");
    let kitty = crossterm::terminal::supports_keyboard_enhancement().unwrap_or(false);
    enter(kitty)?;
    let palette = crate::terminal::probe(1000);
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<AppMsg>();
    let mut app = App::new(opts, config, tx.clone());
    app.mini = true;
    if let Ok(custom) = crate::theme::discover(std::path::Path::new(&app.opts.directory)) {
        app.themes.set_custom(custom);
    }
    if let Some(colors) = &palette {
        if app.themes.lock.is_none()
            && let Some(m) = crate::theme::terminal_mode(colors)
        {
            app.themes.set_mode(m);
        }
        app.themes.set_system_palette(Some(colors.clone()));
    }
    app.reload_theme();

    let client = app.client.clone();
    let sse_tx = tx.clone();
    let (stx, mut srx) = tokio::sync::mpsc::unbounded_channel();
    let retry = crate::api::sse::Retry::mini(app.opts.service.as_ref().map(|s| s.reconnect.clone()));
    tokio::spawn(crate::api::sse::run(client, stx, retry));
    tokio::spawn(async move {
        while let Some(m) = srx.recv().await {
            if sse_tx.send(AppMsg::Sse(m)).is_err() {
                break;
            }
        }
    });

    let resumed = app.opts.session.is_some() || app.opts.continue_last;
    let mut hooks = MiniHooks {
        st: MiniState { replay, replay_limit: limit.unwrap_or(200) as usize, resumed, ..Default::default() },
        footer: Footer { cursor_row: 0, drawn: false },
        pending: Vec::new(),
        splash,
        kitty,
        last_width: crossterm::terminal::size().map(|s| s.0).unwrap_or(80),
    };
    if splash {
        let w = crossterm::terminal::size().map(|s| s.0).unwrap_or(80);
        hooks.pending.extend(lines_to_ansi(&[app.mini_header(w)], w));
    }
    let mut events = crate::app::escape::JoinEscapes::new(EventStream::new());
    let result = crate::app::event_loop::run(&mut app, &mut events, &mut rx, &mut hooks).await;
    let MiniHooks { mut st, mut footer, .. } = hooks;
    // Flush whatever finished, drop the footer, print the exit banner.
    let mut out = String::new();
    footer.erase(&mut out);
    let w = crossterm::terminal::size().map(|s| s.0).unwrap_or(80);
    if st.session.is_some() {
        let (commit, _) = app.mini_transcript(&mut st, w);
        for l in lines_to_ansi(&commit, w) {
            out.push_str(&l);
            out.push_str("\r\n");
        }
    }
    if splash {
        let banner = app.mini_banner();
        if !banner.is_empty() {
            out.push_str("\r\n");
            for l in lines_to_ansi(&banner, w) {
                out.push_str(&l);
                out.push_str("\r\n");
            }
        }
    }
    print!("{out}");
    let _ = std::io::stdout().flush();
    leave(kitty);
    if let Err(e) = result {
        tracing::error!(cause = %e, "tui failed");
        eprintln!("{e}");
        return Ok(1);
    }
    Ok(0)
}

fn run_editor(app: &mut App) {
    let Some(cmd) = std::env::var("VISUAL").ok().filter(|v| !v.is_empty()).or_else(|| std::env::var("EDITOR").ok().filter(|v| !v.is_empty())) else {
        return;
    };
    let path = std::env::temp_dir().join(format!("{}.md", crate::util::locale::now_ms()));
    if std::fs::write(&path, app.editor.input_text()).is_err() {
        return;
    }
    let mut parts = cmd.split(' ').filter(|s| !s.is_empty());
    let program = parts.next().unwrap_or_default().to_string();
    let args: Vec<String> = parts.map(|s| s.to_string()).collect();
    if let Ok(s) = std::process::Command::new(&program).args(&args).arg(&path).current_dir(app.location_dir()).status()
        && s.success()
        && let Ok(text) = std::fs::read_to_string(&path)
        && !text.is_empty()
    {
        let mut info = crate::state::PromptInfo::empty();
        info.text = text.strip_suffix('\n').unwrap_or(&text).to_string();
        app.editor.set(&info);
    }
    let _ = std::fs::remove_file(&path);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn merge() {
        assert_eq!(merge_input(Some("a\n"), Some("b")).as_deref(), Some("a\nb"));
        assert_eq!(merge_input(None, Some("b")).as_deref(), Some("b"));
        assert_eq!(merge_input(Some("  \n"), None), None);
    }
}
