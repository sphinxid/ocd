//! V1 → V2 history import progress badge (upstream `component/migration-overlay.tsx`).
//!
//! One second after the app starts, `GET /api/experimental/migration/v1` is polled every second until it reports
//! `completed`. While the status is `running`, a badge is drawn in the top-right corner (`top=1`, `right=2`,
//! above everything else): a left `┃` border in `text.feedback.info.base` on `background.raised.high`, padding
//! 2×1, and a spinner followed by `<label>[ <numerator>[/<denominator>]]` in `text.feedback.info.base`.
//! Any other status hides it. `error` (or a failed request) stops polling, hides the badge and shows the error
//! toast "Data migration failed" for 10 s. There are no keybindings; the badge is not clickable.

use super::{App, AppMsg, Toast};
use crate::api::error_message;
use crate::theme::Theme;
use crate::ui::text::{SPINNER_FRAMES, fg};
use crate::util::locale::width as sw;
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::Style;
use serde_json::Value;
use std::time::{Duration, Instant};

/// `progress` of a `running` status. Counts keep their raw JSON value (`None` = key absent) so the label is
/// formatted like the upstream template string (an explicit `null` prints as `null`).
#[derive(Clone, Debug, PartialEq)]
pub struct MigrationProgress {
    pub label: String,
    pub numerator: Option<Value>,
    pub denominator: Option<Value>,
}

/// Response of `experimental.migration.v1.status`.
#[derive(Clone, Debug, PartialEq)]
pub enum MigrationStatus {
    Required,
    Completed,
    Running(Option<MigrationProgress>),
    Error(String),
    /// Any other `status` value: treated like `required` (badge hidden, keep polling).
    Other,
}

impl MigrationStatus {
    pub fn parse(v: &Value) -> MigrationStatus {
        match v.get("status").and_then(Value::as_str) {
            Some("required") => MigrationStatus::Required,
            Some("completed") => MigrationStatus::Completed,
            Some("running") => MigrationStatus::Running(v.get("progress").filter(|p| p.is_object()).map(|p| MigrationProgress {
                label: match p.get("label") {
                    None | Some(Value::Null) => String::new(),
                    Some(l) => js_string(l),
                },
                numerator: p.get("numerator").cloned(),
                denominator: p.get("denominator").cloned(),
            })),
            Some("error") => MigrationStatus::Error(js_string(v.get("error").unwrap_or(&Value::Null))),
            _ => MigrationStatus::Other,
        }
    }
}

/// `String(value)` for the JSON values the endpoint can return.
fn js_string(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Null => "undefined".into(),
        Value::Number(n) => match n.as_f64() {
            Some(f) if f.fract() == 0.0 && f.abs() < 1e21 => format!("{}", f as i64),
            _ => n.to_string(),
        },
        other => other.to_string(),
    }
}

/// `count()` of the upstream overlay: `""`, `" <n>"` or `" <n>/<d>"`.
pub fn count(p: &MigrationProgress) -> String {
    let Some(n) = &p.numerator else { return String::new() };
    let n = if n.is_null() { "null".into() } else { js_string(n) };
    match &p.denominator {
        None => format!(" {n}"),
        Some(d) => format!(" {n}/{}", if d.is_null() { "null".into() } else { js_string(d) }),
    }
}

/// The badge text: label plus counts.
pub fn badge_text(p: &MigrationProgress) -> String {
    format!("{}{}", p.label, count(p))
}

impl App {
    /// Start the status poller (`MigrationOverlay` `onMount`). It stops on `completed`, on an error, or once the
    /// app is gone.
    pub fn start_migration_poll(&self) {
        let Ok(handle) = tokio::runtime::Handle::try_current() else { return };
        let client = self.client.clone();
        let tx = self.tx.clone();
        handle.spawn(async move {
            tokio::time::sleep(Duration::from_millis(1000)).await;
            loop {
                let status = match client.migration_v1_status().await {
                    Ok(v) => MigrationStatus::parse(&v),
                    Err(e) => {
                        let message = error_message(&e);
                        let _ = tx.send(AppMsg::Apply(Box::new(move |app: &mut App| app.migration_failed(message))));
                        return;
                    }
                };
                let done = matches!(status, MigrationStatus::Completed | MigrationStatus::Error(_));
                if tx.send(AppMsg::Apply(Box::new(move |app: &mut App| app.on_migration_status(status)))).is_err() {
                    return;
                }
                if done {
                    return;
                }
                tokio::time::sleep(Duration::from_millis(1000)).await;
            }
        });
    }

    /// Apply one polled status.
    pub fn on_migration_status(&mut self, status: MigrationStatus) {
        match status {
            MigrationStatus::Running(p) => self.set_migration_progress(p),
            MigrationStatus::Error(e) => self.migration_failed(if e.is_empty() { "Error".into() } else { e }),
            _ => self.set_migration_progress(None),
        }
    }

    fn set_migration_progress(&mut self, p: Option<MigrationProgress>) {
        let since = match (&self.migration, &p) {
            (Some((_, since)), Some(_)) => *since,
            _ => Instant::now(),
        };
        let next = p.map(|p| (p, since));
        if next.as_ref().map(|(p, _)| p) != self.migration.as_ref().map(|(p, _)| p) {
            self.dirty = true;
        }
        self.migration = next;
    }

    /// The poll loop failed (`error` status or request error).
    pub fn migration_failed(&mut self, message: String) {
        self.set_migration_progress(None);
        self.toast(Toast::new("error", message).title("Data migration failed").duration(10_000));
    }

    /// Draw the badge over everything (`zIndex 10000`, after the reconnect overlay).
    pub(super) fn draw_migration(&mut self, buf: &mut Buffer, area: Rect, t: &Theme) {
        let Some((p, since)) = &self.migration else { return };
        let frame = (since.elapsed().as_millis() / 80) as usize % SPINNER_FRAMES.len();
        let badge = Badge::layout(p, self.config.animations(), area.width as usize);
        let info = fg(t.text.feedback.info.base).bg(t.background.raised.high.tui());
        let blank = Style::default().bg(t.background.raised.high.tui());
        let x0 = area.x as i64 + badge.x;
        for row in 0..3u16 {
            let y = area.y + 1 + row;
            if y >= area.bottom() {
                break;
            }
            for (i, (cell, style)) in badge.row(row, SPINNER_FRAMES[frame], info, blank).into_iter().enumerate() {
                let x = x0 + i as i64;
                if x < area.x as i64 || x >= area.right() as i64 {
                    continue;
                }
                let c = &mut buf[(x as u16, y)];
                c.set_symbol(&cell);
                c.set_style(style);
            }
        }
    }
}

/// Badge geometry, matching what OpenTUI/Yoga lays out for the upstream overlay: an absolutely positioned box
/// (`top 1`, `right 2`) sized to its content, `┃` left border, padding 2 left/right and 1 top/bottom, and a row
/// of [spinner, gap 1, text]. When the text does not fit, it wraps at `width - 7` (the box border, padding, spinner
/// and gap; `width - 5` without animations); the box shrinks to the first wrapped line and only that line shows
/// (the box stays 3 rows tall), and a box wider than `width - 2` extends past the left edge.
#[derive(Clone, Debug, PartialEq)]
pub struct Badge {
    /// Column of the border relative to the area (negative = off-screen).
    pub x: i64,
    pub width: usize,
    pub spinner: bool,
    pub text: String,
}

impl Badge {
    pub fn layout(p: &MigrationProgress, animations: bool, cols: usize) -> Badge {
        let body = if animations { badge_text(p) } else { format!("⋯ {}", badge_text(p)) };
        let lead = if animations { 2 } else { 0 };
        let text = first_line(&body, cols.saturating_sub(5 + lead).max(1));
        let width = 5 + lead + sw(&text);
        Badge { x: cols as i64 - 2 - width as i64, width, spinner: animations, text }
    }

    /// Cells of one of the three rows, left to right, starting at the border column.
    fn row(&self, row: u16, frame: &str, info: Style, blank: Style) -> Vec<(String, Style)> {
        let mut out: Vec<(String, Style)> = vec![("┃".into(), info)];
        out.extend((1..self.width).map(|_| (" ".to_string(), blank)));
        if row == 1 {
            let mut i = 3;
            if self.spinner {
                out[i] = (frame.into(), info);
                i += 2;
            }
            for g in crate::util::locale::graphemes(&self.text) {
                let gw = sw(g);
                if gw == 0 || i >= out.len() {
                    continue;
                }
                out[i] = (g.to_string(), info);
                for k in 1..gw {
                    if i + k < out.len() {
                        out[i + k] = (String::new(), info);
                    }
                }
                i += gw;
            }
        }
        out
    }
}

/// First line of an OpenTUI word wrap at `max` cells: breaks after spaces and `/`, trailing spaces hang past the
/// limit, and a first word longer than the line is broken by characters.
pub fn first_line(text: &str, max: usize) -> String {
    if sw(text) <= max {
        return text.to_string();
    }
    let mut words: Vec<String> = Vec::new();
    let mut cur = String::new();
    let mut in_space = false;
    for g in crate::util::locale::graphemes(text) {
        let space = g == " ";
        if in_space && !space {
            words.push(std::mem::take(&mut cur));
        }
        in_space = space;
        cur.push_str(g);
        if g == "/" {
            words.push(std::mem::take(&mut cur));
        }
    }
    if !cur.is_empty() {
        words.push(cur);
    }
    let mut line = String::new();
    for w in words {
        let fits = sw(&line) + sw(w.trim_end_matches(' ')) <= max;
        if line.is_empty() || fits {
            if line.is_empty() && sw(w.trim_end_matches(' ')) > max {
                let mut out = String::new();
                for g in crate::util::locale::graphemes(&w) {
                    if sw(&out) + sw(g) > max {
                        break;
                    }
                    out.push_str(g);
                }
                return out;
            }
            line.push_str(&w);
        } else {
            break;
        }
    }
    line
}
