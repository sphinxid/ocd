//! The current location and its missing-folder screen (`src/context/location.tsx`,
//! `routes/session/location-missing.tsx`).
//!
//! Location info is fetched with up to 3 tries (500 ms, then 1 s apart) on any error except the
//! server's typed `LocationNotFoundError`; only that error proves the folder is gone, and only for
//! the latest sync of the location still shown. A session whose folder is gone shows "Session
//! location unavailable" in place of the prompt, with "Choose directory" opening the Worktrees
//! dialog to move the session.

use super::{App, Route};
use crate::api::sse::ConnStatus;
use crate::theme::Theme;
use crate::ui::text::*;
use crate::ui::text::{Wrap, wrap};
use crate::util::locale;
use ratatui::style::Style;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

/// Tries and backoff of `retry` (`@opencode/util/retry`).
const ATTEMPTS: u32 = 3;
const DELAY_MS: u64 = 500;
const FACTOR: u64 = 2;

/// Location sync state (`LocationProvider`).
#[derive(Clone, Debug, Default)]
pub struct LocationState {
    /// The location last set (synced when connected).
    pub current: Option<String>,
    /// Bumped by every sync; a stale sync neither retries nor reports.
    pub generation: Arc<AtomicU64>,
    /// The location the server reported missing.
    pub missing: Option<String>,
}

fn not_found(e: &anyhow::Error) -> bool {
    e.downcast_ref::<crate::api::ApiError>().is_some_and(|a| a.tag == "LocationNotFoundError")
}

impl App {
    /// Fetch the location's info with retries; a `LocationNotFoundError` marks it missing.
    pub(super) fn sync_location_info(&mut self, dir: String) {
        let attempt = self.location.generation.fetch_add(1, Ordering::SeqCst) + 1;
        self.location.current = Some(dir.clone());
        self.location.missing = None;
        let generation = self.location.generation.clone();
        let c = self.client.clone();
        let d2 = dir.clone();
        self.spawn(
            async move {
                let mut tries = 0;
                loop {
                    if generation.load(Ordering::SeqCst) != attempt {
                        return Ok(None);
                    }
                    match c.location_get(Some(&d2)).await {
                        Ok(info) => return Ok(Some(info)),
                        Err(e) => {
                            tries += 1;
                            if tries >= ATTEMPTS || not_found(&e) || generation.load(Ordering::SeqCst) != attempt {
                                return Err(e);
                            }
                            tokio::time::sleep(Duration::from_millis(DELAY_MS * FACTOR.pow(tries - 1))).await;
                        }
                    }
                }
            },
            move |app, r| match r {
                Ok(Some(info)) => {
                    app.store.location_mut(&dir).info = Some(info);
                    app.dirty = true;
                }
                Ok(None) => {}
                Err(e) => {
                    if not_found(&e) && app.location.generation.load(Ordering::SeqCst) == attempt && app.location.current.as_deref() == Some(dir.as_str()) {
                        app.location.missing = Some(dir);
                        app.dirty = true;
                    }
                }
            },
        );
    }

    /// Sync the location again when the one shown changed (the session route's
    /// `currentLocation.set(location())`): another session's folder, or a session moved.
    pub fn reconcile_location(&mut self) {
        if self.connection != ConnStatus::Connected || self.location.current.is_none() {
            return;
        }
        if self.location.current.as_deref() != Some(self.location_dir().as_str()) {
            self.sync_location();
        }
    }

    /// The viewed session's folder, when the server reported it missing.
    pub fn location_missing(&self) -> Option<String> {
        let Route::Session(_) = &self.route else { return None };
        let dir = self.session().map(|s| s.location.directory.clone())?;
        (self.location.missing.as_deref() == Some(dir.as_str())).then_some(dir)
    }

    /// `SessionLocationUnavailable` on `SessionQuestion`: the warning header, the folder (home
    /// abbreviated, middle-truncated to 72), the hint, and a single "Choose directory" button.
    pub(super) fn location_missing_lines(&self, t: &Theme, width: usize, dir: &str) -> Vec<SLine> {
        let s = |text: &str, c| Seg::new(text.to_string(), fg(c));
        let bg = t.background.raised.base;
        let bar = Seg::new("┃", Style::default().fg(t.background.action.primary.focused.tui()).bg(bg.tui()));
        let inner = width.saturating_sub(6).max(1);
        let mut body: Vec<SLine> = vec![SLine::new(vec![pad(1), s("△", t.text.feedback.warning.base), s(" Session location unavailable", t.text.base)]), SLine::blank()];
        let shown = locale::truncate_middle(&locale::abbreviate_home(dir), 72);
        body.extend(wrap(&[s(&shown, t.text.muted)], inner, Wrap::Word).into_iter().map(|x| SLine::new(x).prefixed(&[pad(1)])));
        body.push(SLine::blank());
        body.extend(wrap(&[s("Choose another directory to continue this session.", t.text.base)], inner, Wrap::Word).into_iter().map(|x| SLine::new(x).prefixed(&[pad(1)])));
        let mut out: Vec<SLine> = vec![SLine::new(vec![bar.clone()]).with_bg(Some(bg.tui()))];
        for l in body.into_iter().take(13) {
            out.push(l.prefixed(&[bar.clone(), pad(1)]).with_bg(Some(bg.tui())));
        }
        out.push(SLine::new(vec![bar.clone()]).with_bg(Some(bg.tui())));
        // Footer band: the button on the left, `enter confirm` on the right (stacked below 80 columns).
        let band = t.dec(bg);
        let button = vec![Seg::new(" Choose directory ", fg_bg(t.text.action.primary.focused, t.background.action.primary.focused))];
        let hint = vec![s("enter", t.text.base), s(" confirm", t.text.muted)];
        let click = Some(Click::Custom("location.move".into()));
        let band_line = |segs: Vec<Seg>| SLine::new(segs).prefixed(&[bar.clone(), pad(2)]).with_bg(Some(band.tui()));
        out.push(SLine::new(vec![bar.clone()]).with_bg(Some(band.tui())));
        if self.size.0 < 80 {
            out.push(band_line(button).on_click(click));
            out.push(band_line(hint));
        } else {
            let lw: usize = button.iter().map(|x| x.width()).sum();
            let rw: usize = hint.iter().map(|x| x.width()).sum();
            let mut segs = button;
            segs.push(pad(width.saturating_sub(3 + 3 + lw + rw)));
            segs.extend(hint);
            out.push(band_line(segs).on_click(click));
        }
        out.push(SLine::new(vec![bar]).with_bg(Some(band.tui())));
        out
    }

    /// Keys while the missing-folder screen replaces the prompt: Enter chooses a directory.
    pub(super) fn location_missing_key(&mut self, s: &crate::keymap::Stroke) -> bool {
        if s.name == "return" && !s.ctrl && !s.meta && !s.shift {
            self.recover_location();
            return true;
        }
        false
    }

    /// "Choose directory": the Worktrees dialog for the session's project; selecting a row moves
    /// the session there.
    pub fn recover_location(&mut self) {
        let Some(project) = self.session().map(|s| s.project_id.clone()).filter(|p| !p.is_empty()) else { return };
        self.dialogs.clear();
        self.open_worktrees_dialog_for(project);
    }
}

fn pad(n: usize) -> Seg {
    Seg::new(" ".repeat(n), Style::default())
}
