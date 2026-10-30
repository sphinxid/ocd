//! The `/goal` status (`feature-plugins/prompt/goal.tsx`): the routed session's goal from the goal
//! plugin's RPC (`opencode.goal` `get`), fetched when the session changes or the server reconnects and
//! kept current from `rpc.opencode.goal.changed`; the `◎ goal` item in the prompt footer and the goal
//! dialog (pause, resume, clear through the `goal` server command).

use super::{App, Toast};
use crate::ui::dialog::{Body, DIALOG_INSET, Dialog, Footer, FooterItem, Size, TextDialog};
use crate::ui::text::*;
use serde::Deserialize;
use serde_json::{Value, json};

pub const PURPOSE: &str = "goal";
pub const CLICK: &str = "goal.show";
pub const RPC: &str = "opencode.goal";
pub const CHANGED: &str = "rpc.opencode.goal.changed";

/// `Goal.State`.
#[derive(Clone, Debug, Default, PartialEq, Deserialize)]
pub struct GoalState {
    #[serde(default)]
    pub id: String,
    #[serde(default)]
    pub objective: String,
    /// `active`, `paused` or `complete`.
    #[serde(default)]
    pub status: String,
    #[serde(default)]
    pub reason: Option<String>,
}

/// The goal of the routed session and the bookkeeping that orders fetches and change events.
#[derive(Clone, Debug, Default)]
pub struct GoalTracker {
    /// The session the goal belongs to (the routed session when it was last checked).
    pub session: Option<String>,
    pub goal: Option<GoalState>,
    /// A change that arrives while a fetch is in flight is newer than its reply, so each change or fetch takes a turn.
    pub turn: u64,
}

fn parse(v: &Value) -> Option<GoalState> {
    if v.is_null() {
        return None;
    }
    serde_json::from_value(v.clone()).ok()
}

impl App {
    /// The routed session's goal.
    pub fn goal(&self) -> Option<&GoalState> {
        if self.goal.session.as_deref() == self.session_id() { self.goal.goal.as_ref() } else { None }
    }

    fn goal_load(&mut self, id: &str) {
        self.goal.turn += 1;
        let current = self.goal.turn;
        let directory = self.store.session(id).map(|s| s.location.directory.clone()).filter(|d| !d.is_empty());
        let c = self.client.clone();
        let sid = id.to_string();
        // A server without the goal plugin answers with an error, and the session shows no goal.
        self.spawn(async move { c.rpc_call(RPC, "get", json!({ "sessionID": sid }), directory.as_deref()).await }, {
            let id = id.to_string();
            move |app, r| {
                if let Ok(v) = r
                    && current == app.goal.turn
                    && app.session_id() == Some(id.as_str())
                {
                    app.goal.goal = parse(&v);
                    app.goal_changed();
                }
            }
        });
    }

    /// Periodic check: a session change resets the goal and fetches the new session's.
    pub(super) fn goal_tick(&mut self) {
        let sid = self.session_id().map(str::to_string);
        if self.goal.session == sid {
            return;
        }
        self.goal.session = sid.clone();
        self.goal.goal = None;
        if let Some(id) = sid {
            self.goal_load(&id);
        }
        self.goal_changed();
    }

    /// `server.connected`: changes published meanwhile were missed.
    pub(super) fn goal_reconnected(&mut self) {
        if let Some(id) = self.session_id().map(str::to_string) {
            self.goal.session = Some(id.clone());
            self.goal_load(&id);
        }
    }

    /// `rpc.opencode.goal.changed`.
    pub(super) fn goal_event(&mut self, data: &Value) {
        let sid = data.get("sessionID").and_then(|s| s.as_str());
        if sid.is_none() || sid != self.session_id() {
            return;
        }
        self.goal.turn += 1;
        self.goal.session = sid.map(str::to_string);
        self.goal.goal = data.get("goal").and_then(parse);
        self.goal_changed();
    }

    fn goal_changed(&mut self) {
        self.refresh_goal_dialog();
        self.dirty = true;
    }

    /// The footer item: `◎ goal`, `◎ goal paused` or `◎ goal met`.
    pub(super) fn goal_item(&self, t: &crate::theme::Theme) -> Option<Seg> {
        let g = self.goal()?;
        let (label, color) = match g.status.as_str() {
            "paused" => ("goal paused", t.text.feedback.warning.base),
            "complete" => ("goal met", t.text.feedback.success.base),
            _ => ("goal", t.text.muted),
        };
        Some(seg(format!("◎ {label}"), fg(color)))
    }

    /// A click on the footer item: the goal dialog, or how to start one.
    pub(super) fn goal_click(&mut self) {
        if self.goal().is_none() {
            self.toast(Toast::new("info", "No goal is set. Start one with /goal <objective>."));
            return;
        }
        let Some(sid) = self.session_id().map(str::to_string) else { return };
        let mut d = Dialog::text(PURPOSE, TextDialog::new("Goal", vec![]));
        d.size = Size::Medium;
        d.data = json!(sid);
        self.open_dialog(d);
        self.refresh_goal_dialog();
    }

    /// Rebuild the open goal dialog from the goal.
    pub(super) fn refresh_goal_dialog(&mut self) {
        if self.dialogs.last().is_none_or(|d| d.purpose != PURPOSE) {
            return;
        }
        let t = self.theme().dialog().clone();
        let w = (Size::Medium.width().min(self.size.0.saturating_sub(2)).max(10) as usize).saturating_sub(DIALOG_INSET * 2).max(1);
        let td = match self.goal() {
            None => {
                let mut td = TextDialog::new("", vec![]);
                td.empty = true;
                td
            }
            Some(g) => {
                let mut lines: Vec<SLine> = wrap(&[seg(g.objective.clone(), fg(t.text.base))], w, Wrap::Word).into_iter().map(SLine::new).collect();
                lines.push(SLine::blank());
                let (status, color) = match g.status.as_str() {
                    "active" => ("Active", t.text.muted),
                    "paused" => ("Paused", t.text.feedback.warning.base),
                    _ => ("Met", t.text.feedback.success.base),
                };
                lines.push(SLine::new(clip(&[seg(format!("◎ {status}"), fg(color))], w)));
                if let Some(reason) = g.reason.as_deref().filter(|r| !r.is_empty()) {
                    lines.extend(wrap(&[seg(reason, fg(t.text.muted))], w, Wrap::Word).into_iter().map(SLine::new));
                }
                let mut primary = Vec::new();
                if g.status == "active" {
                    primary.push(FooterItem::new("pause", "p"));
                }
                if g.status == "paused" {
                    primary.push(FooterItem::new("resume", "r"));
                }
                primary.push(FooterItem::new("clear", "x"));
                TextDialog::new("Goal", lines).footer(Footer { primary, ..Default::default() })
            }
        };
        if let Some(d) = self.dialogs.last_mut() {
            d.body = Body::Text(Box::new(td));
        }
    }

    /// `p` / `r` / `x` in the goal dialog.
    pub(super) fn goal_key(&mut self, key: &str, sid: &str) {
        let Some(status) = self.goal().map(|g| g.status.clone()) else { return };
        let text = match key {
            "p" if status == "active" => "pause",
            "r" if status == "paused" => "resume",
            "x" => "clear",
            _ => return,
        };
        if text == "clear" {
            self.dialogs.clear();
        }
        let c = self.client.clone();
        let sid = sid.to_string();
        self.spawn(async move { c.command(&sid, json!({ "name": "goal", "text": text })).await }, |app, r| {
            if let Err(e) = r {
                app.toast(Toast::new("error", crate::api::error_message(&e)).title("Failed to update the goal"));
            }
        });
    }
}
