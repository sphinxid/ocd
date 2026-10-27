//! The TUI event loop: input first, batched message processing, capped frame rate.
//!
//! Each iteration waits for anything (terminal input, a queued message — SSE event or async
//! result —, the animation tick, or the next frame deadline), then drains everything that is
//! ready: all pending terminal input first, then queued messages in batches, re-checking input
//! between batches and stopping after a time budget so input never waits behind a backlog. A
//! frame is drawn at most once per [`FRAME`], and only when something changed or an animation is
//! visible. Drawing is decoupled from the number of events: a flood of streaming deltas costs one
//! frame per [`FRAME`], not one frame per delta.

use super::{App, AppMsg};
use crate::keymap;
use crossterm::event::Event as CEvent;
use futures::{Stream, StreamExt};
use std::task::Poll;
use std::time::{Duration, Instant};
use tokio::sync::mpsc::UnboundedReceiver;

/// Minimum time between two frames (~60 fps).
pub const FRAME: Duration = Duration::from_millis(16);
/// Animation tick (spinners, the busy line's timer, toasts, leader timeouts).
pub const TICK: Duration = Duration::from_millis(40);
/// Longest stretch spent applying queued messages before input is checked again.
const BUDGET: Duration = Duration::from_millis(8);
/// Messages applied between two input checks.
const BATCH: usize = 64;

/// Terminal-specific steps of the loop (full screen, mini mode, tests).
pub trait Hooks {
    /// Draw a frame.
    fn draw(&mut self, app: &mut App) -> anyhow::Result<()>;
    /// Something besides the app needs a frame (e.g. lines waiting to be printed).
    fn wants_draw(&self) -> bool {
        false
    }
    /// `app.editor_requested` is set.
    fn editor(&mut self, app: &mut App) -> anyhow::Result<()> {
        app.editor_requested = false;
        Ok(())
    }
    /// `app.suspend_requested` is set.
    fn suspend(&mut self, app: &mut App) -> anyhow::Result<()> {
        app.suspend_requested = false;
        Ok(())
    }
    /// A terminal event; return `true` when it was handled here instead of by the app.
    fn intercept(&mut self, _app: &mut App, _ev: &CEvent) -> bool {
        false
    }
}

/// Called after every frame.
pub type AfterFrame<'a> = Box<dyn FnMut(&mut App) + 'a>;

/// Draws into a ratatui terminal (full-screen TUI, tests).
pub struct Screen<'a, B: ratatui::backend::Backend> {
    pub term: &'a mut ratatui::Terminal<B>,
    pub after: Option<AfterFrame<'a>>,
}

impl<B: ratatui::backend::Backend> Hooks for Screen<'_, B>
where
    B::Error: Send + Sync + 'static,
{
    fn draw(&mut self, app: &mut App) -> anyhow::Result<()> {
        self.term.draw(|f| app.draw(f))?;
        if let Some(f) = &mut self.after {
            f(app);
        }
        Ok(())
    }
}

/// Apply one terminal event.
pub fn on_term(app: &mut App, ev: CEvent) {
    match ev {
        CEvent::Key(k) => {
            if let Some(s) = keymap::stroke_from_event(&k) {
                app.on_key(s);
            }
        }
        CEvent::Paste(text) => app.paste(&text),
        CEvent::Mouse(m) => app.on_mouse(m),
        CEvent::Resize(_, _) => app.dirty = true,
        CEvent::FocusGained => app.focused = Some(true),
        CEvent::FocusLost => app.focused = Some(false),
    }
}

/// Apply one queued message.
pub fn on_msg(app: &mut App, msg: AppMsg) {
    match msg {
        AppMsg::Sse(m) => app.on_sse(m),
        AppMsg::Apply(f) => f(app),
    }
}

/// The next item of `s` if one is ready now (registers the task's waker otherwise).
async fn ready<S: Stream + Unpin>(s: &mut S) -> Option<Option<S::Item>> {
    match futures::poll!(s.next()) {
        Poll::Ready(x) => Some(x),
        Poll::Pending => None,
    }
}

enum Input {
    Open,
    Closed,
}

async fn drain_input<S, H>(app: &mut App, events: &mut S, hooks: &mut H) -> anyhow::Result<Input>
where
    S: Stream<Item = std::io::Result<CEvent>> + Unpin,
    H: Hooks,
{
    while let Some(ev) = ready(events).await {
        match ev {
            Some(Ok(ev)) => {
                if !hooks.intercept(app, &ev) {
                    on_term(app, ev);
                }
            }
            Some(Err(e)) => return Err(e.into()),
            None => return Ok(Input::Closed),
        }
        if app.quit {
            break;
        }
    }
    Ok(Input::Open)
}

/// Run until the app quits, the input stream ends or a message channel closes.
pub async fn run<S, H>(app: &mut App, events: &mut S, rx: &mut UnboundedReceiver<AppMsg>, hooks: &mut H) -> anyhow::Result<()>
where
    S: Stream<Item = std::io::Result<CEvent>> + Unpin,
    H: Hooks,
{
    let mut tick = tokio::time::interval(TICK);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let mut last_draw: Option<Instant> = None;
    // Redraw requested by the tick for a visible animation.
    let mut animate = true;
    loop {
        // Rows of a long transcript still rendering in the background also take frames.
        if (app.dirty || animate || hooks.wants_draw() || app.transcript_pending()) && last_draw.is_none_or(|t| t.elapsed() >= FRAME) {
            // The location shown changed (another session's folder, a moved session): sync it.
            app.reconcile_location();
            hooks.draw(app)?;
            app.dirty = false;
            animate = false;
            last_draw = Some(Instant::now());
        }
        if app.quit {
            break;
        }
        if app.editor_requested {
            hooks.editor(app)?;
            continue;
        }
        if app.suspend_requested {
            hooks.suspend(app)?;
            continue;
        }
        let pending_draw = app.dirty || animate || hooks.wants_draw() || app.transcript_pending();
        let deadline = last_draw.map(|t| t + FRAME).unwrap_or_else(Instant::now);
        tokio::select! {
            biased;
            ev = events.next() => match ev {
                Some(Ok(ev)) => {
                    if !hooks.intercept(app, &ev) {
                        on_term(app, ev);
                    }
                }
                Some(Err(e)) => return Err(e.into()),
                None => break,
            },
            _ = tick.tick() => {
                app.tick();
                if app.animating() {
                    animate = true;
                }
            }
            _ = tokio::time::sleep_until(deadline.into()), if pending_draw => {}
            msg = rx.recv() => match msg {
                Some(m) => on_msg(app, m),
                None => break,
            },
        }
        // Drain what is ready: input first, then queued messages in bounded batches.
        let started = Instant::now();
        loop {
            if let Input::Closed = drain_input(app, events, hooks).await? {
                return Ok(());
            }
            if app.quit || app.editor_requested || app.suspend_requested {
                break;
            }
            let mut n = 0;
            while n < BATCH
                && let Ok(m) = rx.try_recv()
            {
                on_msg(app, m);
                n += 1;
            }
            if n < BATCH || started.elapsed() >= BUDGET {
                break;
            }
        }
    }
    Ok(())
}
