//! Update notification and `/update` (upstream `context/update-notification.tsx`, `component/dialog-update.tsx`,
//! the `UpdateNotification` row of `routes/home.tsx` and the `opencode.update` command of `app.tsx`).
//!
//! The CLI runs the update check in the background and hands its results over [`UpdaterHooks::notices`]
//! (`updater.subscribe`). The home screen shows the newest one under the logo, `/update to install vX`, or
//! `/exit restart to use vX` after an automatic install, until "Skip" in the dialog dismisses it; dismissed
//! versions are kept in the TUI's `update-notifications` store (the last 100, as `client:<version>`). Clicking
//! it opens the dialog (or exits after an install). `/update` opens the dialog too: without a known update it
//! looks the latest release up (`check`), Enter installs it (`apply`), and once installed Enter quits. After an
//! install, the exit epilogue says which version to run.

use super::App;
use crate::ui::dialog::{Body, Dialog, Footer, FooterItem, Size, TextDialog};
use crate::ui::text::{SLine, Seg, Wrap, fg, seg, wrap_lines};
use futures::future::BoxFuture;
use std::sync::Arc;

/// `ClientNotice`: what the background check found.
#[derive(Clone, Debug, PartialEq)]
pub enum UpdateNotice {
    /// A newer release exists (policy `notify`).
    Available(String),
    /// The check installed it (policy `auto`).
    Installed(String),
}

impl UpdateNotice {
    pub fn version(&self) -> &str {
        match self {
            UpdateNotice::Available(v) | UpdateNotice::Installed(v) => v,
        }
    }
}

/// `updater.check()`: `None` when already up to date.
#[derive(Clone, Debug, PartialEq)]
pub enum UpdateCheck {
    Available(String),
    Installed(String),
    Unavailable(String),
}

pub type CheckHook = Arc<dyn Fn() -> BoxFuture<'static, anyhow::Result<Option<UpdateCheck>>> + Send + Sync>;
pub type ApplyHook = Arc<dyn Fn(String) -> BoxFuture<'static, anyhow::Result<()>> + Send + Sync>;

/// `TuiInput.updater` (`UpdateSource`).
#[derive(Clone)]
pub struct UpdaterHooks {
    /// The background check's latest result (`subscribe`).
    pub notices: tokio::sync::watch::Receiver<Option<UpdateNotice>>,
    /// Looks the latest release up now.
    pub check: CheckHook,
    /// Installs a version over the running executable.
    pub apply: ApplyHook,
}

impl std::fmt::Debug for UpdaterHooks {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("UpdaterHooks")
    }
}

/// `UpdateState` plus the dialog's own states (`checking`, `check-failed`, `unavailable`, `current`).
#[derive(Clone, Debug, PartialEq)]
pub enum UpdateState {
    Checking,
    Current,
    Available(String),
    Installing(String),
    Installed(String),
    Failed(String),
    CheckFailed(String),
    Unavailable(String),
}

#[derive(Clone, Debug, Default)]
pub struct UpdateUi {
    /// The notice on the home screen.
    pub notification: Option<UpdateNotice>,
    /// `state()`: available, installing, installed or failed.
    pub state: Option<UpdateState>,
    /// What the open dialog's own check shows (checking, check-failed, unavailable); `None` once it is done.
    pub check: Option<UpdateState>,
    /// The version installed while the TUI ran (`epilogue.updatedVersion`).
    pub updated_version: Option<String>,
}

pub const PURPOSE: &str = "update";
pub const NOTIFICATION_CLICK: &str = "update.notification";
const STORE: &str = "update-notifications";

impl App {
    /// Forwards the background check's results (`updater.subscribe`).
    pub fn start_update_watch(&self) {
        let Some(hooks) = self.opts.updater.clone() else { return };
        let Ok(handle) = tokio::runtime::Handle::try_current() else { return };
        let tx = self.tx.clone();
        let mut rx = hooks.notices;
        handle.spawn(async move {
            loop {
                let notice = rx.borrow_and_update().clone();
                if let Some(n) = notice
                    && tx.send(super::AppMsg::Apply(Box::new(move |app: &mut App| app.update_notify(n)))).is_err()
                {
                    return;
                }
                if rx.changed().await.is_err() {
                    return;
                }
            }
        });
    }

    fn update_dismissed(&self) -> Vec<String> {
        self.keyed
            .get(STORE)
            .and_then(|v| v.get("versions").cloned())
            .and_then(|v| serde_json::from_value::<Vec<String>>(v).ok())
            .unwrap_or_default()
    }

    /// `notify(notice)`
    pub fn update_notify(&mut self, notice: UpdateNotice) {
        if self.opts.updater.is_none() {
            return;
        }
        // Every way out of the TUI after a local install tells the user to start the new version.
        if let UpdateNotice::Installed(v) = &notice {
            self.update.updated_version = Some(v.clone());
        }
        let dismissed = self.update_dismissed();
        let v = notice.version();
        if dismissed.iter().any(|d| *d == format!("client:{v}") || d == v) {
            return;
        }
        self.update.notification = Some(notice);
        self.dirty = true;
    }

    /// `dismiss()`: hides the notification and remembers its version.
    pub fn update_dismiss(&mut self) {
        let Some(current) = self.update.notification.take() else { return };
        let key = format!("client:{}", current.version());
        self.keyed.update(STORE, |v| {
            let mut versions: Vec<String> =
                v.get("versions").cloned().and_then(|x| serde_json::from_value(x).ok()).unwrap_or_default();
            versions.push(key);
            let skip = versions.len().saturating_sub(100);
            *v = serde_json::json!({ "versions": versions[skip..] });
        });
        self.dirty = true;
    }

    /// The notification row was clicked: quit after an install, otherwise open the dialog.
    pub fn update_notification_click(&mut self) {
        match &self.update.notification {
            Some(UpdateNotice::Installed(_)) => self.quit = true,
            Some(UpdateNotice::Available(_)) => self.open_update(true),
            None => {}
        }
    }

    /// `open(origin)`: `/update` (manual) or the notification.
    pub fn open_update(&mut self, from_notification: bool) {
        if self.opts.updater.is_none() {
            return;
        }
        let known = self.update.notification.clone();
        if from_notification && known.is_none() {
            return;
        }
        // The notification can predate an installation through /update.
        if let Some(k) = &known {
            let installing = matches!(self.update.state, Some(UpdateState::Installing(_)));
            let installed_same = matches!(&self.update.state, Some(UpdateState::Installed(v)) if v == k.version());
            if !installing && !installed_same {
                self.update.state = Some(match k {
                    UpdateNotice::Available(v) => UpdateState::Available(v.clone()),
                    UpdateNotice::Installed(v) => UpdateState::Installed(v.clone()),
                });
            }
        }
        let needs_check = matches!(self.update.state, None | Some(UpdateState::Failed(_)));
        self.update.check = needs_check.then_some(UpdateState::Checking);
        let mut d = self.update_dialog();
        d.centered = true;
        self.open_dialog(d);
        if needs_check {
            self.update_check();
        }
    }

    fn update_check(&mut self) {
        let Some(hooks) = self.opts.updater.clone() else { return };
        self.spawn(async move { (hooks.check)().await }, |app, result| {
            if !matches!(app.update.check, Some(UpdateState::Checking)) {
                return;
            }
            app.update.check = None;
            match result {
                Err(e) => app.update.check = Some(UpdateState::CheckFailed(crate::api::error_message(&e))),
                Ok(Some(UpdateCheck::Unavailable(m))) => app.update.check = Some(UpdateState::Unavailable(m)),
                Ok(Some(UpdateCheck::Installed(v))) => {
                    app.update.updated_version = Some(v.clone());
                    app.update.state = Some(UpdateState::Installed(v));
                }
                Ok(Some(UpdateCheck::Available(v))) => app.update.state = Some(UpdateState::Available(v)),
                Ok(None) => app.update.state = None,
            }
            app.refresh_update_dialog();
        });
    }

    /// The state the dialog shows.
    pub fn update_display(&self) -> UpdateState {
        if let Some(c @ UpdateState::CheckFailed(_)) = &self.update.check {
            return c.clone();
        }
        if let Some(s @ UpdateState::Installing(_)) = &self.update.state {
            return s.clone();
        }
        if let Some(c) = &self.update.check {
            return c.clone();
        }
        self.update.state.clone().unwrap_or(UpdateState::Current)
    }

    /// The dialog's buttons: "skip", then "update" or "restart" (none while installing).
    pub fn update_buttons(&self) -> Vec<&'static str> {
        match self.update_display() {
            UpdateState::Installing(_) => vec![],
            UpdateState::Available(_) => vec!["skip", "update"],
            UpdateState::Installed(_) => vec!["skip", "restart"],
            _ => vec!["skip"],
        }
    }

    /// `DialogUpdate`
    pub fn update_dialog(&self) -> Dialog {
        let t = self.theme().clone();
        let state = self.update_display();
        let title = match state {
            UpdateState::Installing(_) => "Updating ocd",
            UpdateState::Available(_) | UpdateState::Failed(_) => "Update available",
            _ => "Update",
        };
        let (text, color) = match &state {
            UpdateState::Checking => ("Checking for updates…".to_string(), t.text.base),
            UpdateState::Available(_) => {
                ("An update is available. After installing, you'll be prompted to restart ocd.".to_string(), t.text.muted)
            }
            UpdateState::Installing(v) => (format!("Installing ocd {v}…"), t.text.base),
            UpdateState::Installed(_) => {
                ("Update successful! Restart closes ocd, then run ocd to use the new version.".to_string(), t.text.muted)
            }
            UpdateState::Current => ("ocd is already up to date.".to_string(), t.text.muted),
            UpdateState::Unavailable(m) => (m.clone(), t.text.muted),
            UpdateState::Failed(m) | UpdateState::CheckFailed(m) => (m.clone(), t.text.feedback.error.base),
        };
        let width = (Size::Medium.width().min(self.size.0.saturating_sub(2)) as usize).saturating_sub(6).max(10);
        let lines: Vec<SLine> = wrap_lines(&[seg(text, fg(color))], width, Wrap::Word, &[]);
        let buttons = self.update_buttons();
        let primary = buttons
            .iter()
            .enumerate()
            .map(|(i, b)| FooterItem::new(*b, if i + 1 == buttons.len() { "enter" } else { "" }))
            .collect();
        let mut td = TextDialog::new(title, lines).footer(Footer { primary, ..Default::default() });
        td.empty_footer_gap = true;
        let mut d = Dialog::text(PURPOSE, td);
        d.centered = true;
        d
    }

    /// Redraws an open update dialog after a state change.
    pub fn refresh_update_dialog(&mut self) {
        let fresh = self.update_dialog();
        if let Some(d) = self.dialogs.iter_mut().find(|d| d.purpose == PURPOSE) {
            d.body = fresh.body;
        }
        self.dirty = true;
    }

    /// Enter in the dialog: its last button.
    pub fn update_dialog_key(&mut self, key: &str) {
        if key != "return" {
            return;
        }
        match self.update_buttons().last().copied() {
            Some("update") => self.update_install(),
            Some("restart") => {
                if matches!(self.update.state, Some(UpdateState::Installed(_))) {
                    self.quit = true;
                }
            }
            Some("skip") => {
                self.update_dismiss();
                self.close_update_dialog();
            }
            _ => {}
        }
    }

    fn close_update_dialog(&mut self) {
        self.dialogs.retain(|d| d.purpose != PURPOSE);
        self.update.check = None;
        self.dirty = true;
    }

    /// `install()`
    pub fn update_install(&mut self) {
        let Some(hooks) = self.opts.updater.clone() else { return };
        let Some(UpdateState::Available(version)) = self.update.state.clone() else { return };
        self.update.state = Some(UpdateState::Installing(version.clone()));
        self.refresh_update_dialog();
        let v = version.clone();
        self.spawn(async move { (hooks.apply)(v).await }, move |app, result| {
            app.update.state = Some(match result {
                Ok(()) => {
                    app.update.updated_version = Some(version.clone());
                    UpdateState::Installed(version)
                }
                Err(e) => UpdateState::Failed(crate::api::error_message(&e)),
            });
            app.refresh_update_dialog();
        });
    }

    /// The home row under the logo: `(segments, width)`, `None` without a notification.
    pub fn update_notification_segs(&self) -> Option<Vec<Seg>> {
        let n = self.update.notification.as_ref()?;
        let t = self.theme();
        let (cmd, rest) = match n {
            UpdateNotice::Installed(v) => ("/exit", format!(" restart to use v{v}")),
            UpdateNotice::Available(v) => ("/update", format!(" to install v{v}")),
        };
        Some(vec![seg(cmd, fg(t.text.action.primary.selected)), seg(rest, fg(t.text.muted))])
    }

    /// What the TUI prints when it closes: the session epilogue, with the installed version.
    pub fn exit_epilogue(&self) -> Option<String> {
        let Some(v) = &self.update.updated_version else { return self.epilogue.clone() };
        let session = if self.epilogue.is_some() {
            self.session().map(|s| (crate::util::locale::truncate(&s.display_title(), 50), s.id.clone()))
        } else {
            None
        };
        Some(crate::ui::view_helpers::epilogue(session.as_ref().map(|(t, id)| (t.as_str(), id.as_str())), Some(v)))
    }
}

/// `Body::Text` of the update dialog (tests).
pub fn dialog_text(d: &Dialog) -> String {
    match &d.body {
        Body::Text(t) => t.lines.iter().map(|l| l.segs.iter().map(|s| s.text.as_str()).collect::<String>()).collect::<Vec<_>>().join("\n"),
        _ => String::new(),
    }
}
