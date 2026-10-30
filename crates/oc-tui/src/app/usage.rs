//! OpenCode Console usage limits (`component/usage.tsx`, `feature-plugins/sidebar/limit.tsx`): the
//! location's limits (`useUsageLimits`), `/usage` and its per-model view, the prompt's limit label, the
//! sidebar block, the once-per-process warnings and the model picker's `Usage` action. The limits load
//! lazily: the first reader (`/usage`, the model picker, or a Console-served model in the prompt) loads
//! them, and events reload them only where they were loaded (`store::Catalog::UsageLimits`).

use super::{App, Route, Toast};
use crate::api::types::ModelInfo;
use crate::store::Catalog;
use crate::ui::dialog::{Body, DIALOG_INSET, Dialog, Opt, Select, Size, TextDialog};
use crate::ui::text::*;
use crate::util::usage_limits::{self as ul, Info, Level, ModelUsage};
use serde_json::json;

pub const PURPOSE: &str = "usage";
pub const MODEL_PURPOSE: &str = "usage-model";
pub const SIDEBAR_ID: &str = "opencode.sidebar.limit";


/// `limitLabel`: how a model's limits read where they are short on space; nothing while there's plenty.
pub fn limit_label(usage: Option<&ModelUsage>, model: &ModelInfo) -> Option<(String, bool)> {
    let usage = usage?;
    match ul::usage_level(usage, model) {
        Level::Plenty => None,
        Level::Reached => Some(("Limit reached".into(), true)),
        level => Some((format!("{} left", ul::format_dollars(usage.left)), level == Level::Out)),
    }
}

impl App {
    /// `limits.info()`: `None` until loaded, `Some(None)` when the location isn't signed in.
    pub fn usage_state(&self) -> Option<Option<&Info>> {
        self.loc().and_then(|l| l.usage_limits.as_ref()).map(|v| v.as_ref())
    }

    /// The loaded limits of a signed-in location.
    pub fn usage_info(&self) -> Option<&Info> {
        self.usage_state().flatten()
    }

    /// `limits.load(refresh)`: load once, or again with `refresh`.
    pub fn usage_load(&mut self, refresh: bool) {
        let dir = self.loc_dir();
        if refresh || !self.store.catalog_active(&dir, Catalog::UsageLimits) {
            self.fetch_catalog(&dir, Catalog::UsageLimits);
        }
    }

    /// The directory the location data is read from (`loc()`).
    fn loc_dir(&self) -> String {
        let d = self.location_dir();
        if self.store.locations.contains_key(&d) || !self.store.locations.contains_key(&self.opts.directory) { d } else { self.opts.directory.clone() }
    }

    /// `limits.usage(model)`.
    pub fn model_usage(&self, model: &ModelInfo) -> Option<ModelUsage> {
        let info = self.usage_info()?;
        let providers = self.loc().and_then(|l| l.providers.as_deref()).unwrap_or(&[]);
        ul::model_usage(info, model, providers.iter().find(|p| p.id == model.provider_id))
    }

    /// `limits.served(providerID)`.
    pub fn served_by_console(&self, provider: &str) -> bool {
        let providers = self.loc().and_then(|l| l.providers.as_deref()).unwrap_or(&[]);
        ul::served_by_console(providers.iter().find(|p| p.id == provider))
    }

    /// The first shortcut of `model.list`, or `/models` when it has none.
    pub fn models_key(&self) -> String {
        let k = self.keybinds.shortcut("model.list");
        if k.is_empty() { "/models".into() } else { k }
    }

    fn selected_model_info(&self) -> Option<ModelInfo> {
        let m = self.current_model()?;
        self.models_ref().iter().find(|x| x.provider_id == m.provider_id && x.id == m.id).cloned()
    }

    /// The prompt's limit label for the selected model.
    pub fn prompt_limit(&self) -> Option<(String, bool)> {
        let model = self.selected_model_info()?;
        limit_label(self.model_usage(&model).as_ref(), &model)
    }

    /// The `opencode.sidebar.limit` block: what the limits leave for the selected model.
    pub fn sidebar_limit_lines(&self, t: &crate::theme::Theme) -> Vec<SLine> {
        if !self.builtin_active(SIDEBAR_ID) {
            return vec![];
        }
        let Some(model) = self.selected_model_info() else { return vec![] };
        let Some(usage) = self.model_usage(&model) else { return vec![] };
        let danger = matches!(ul::usage_level(&usage, &model), Level::Out | Level::Reached);
        vec![
            SLine::new(vec![Seg::new("Limit", bold(fg(t.text.base)))]),
            SLine::new(vec![seg(format!("{} remaining", ul::format_dollars(usage.left)), fg(if danger { t.text.feedback.error.base } else { t.text.muted }))]),
            SLine::blank(),
        ]
    }

    /// Periodic work: the prompt loads the limits once for a Console-served model, the session route
    /// warns as the selected model runs low, and the open usage dialogs follow the data.
    pub(super) fn usage_tick(&mut self) {
        if let Some(m) = self.current_model()
            && self.served_by_console(&m.provider_id)
            && self.usage_state().is_none()
            && !self.store.catalog_active(&self.loc_dir(), Catalog::UsageLimits)
        {
            self.usage_load(false);
        }
        if matches!(self.route, Route::Session(_)) {
            self.usage_alerts();
        }
    }

    /// `useUsageAlerts`: warn once as the selected model runs low and once at its limit.
    fn usage_alerts(&mut self) {
        let Some(model) = self.selected_model_info() else { return };
        let Some(usage) = self.model_usage(&model) else { return };
        let level = ul::usage_level(&usage, &model);
        if level == Level::Plenty {
            return;
        }
        let key = format!("{}/{}:{}", model.provider_id, model.id, if level == Level::Reached { "reached" } else { "low" });
        if !self.usage_warned.insert(key) {
            return;
        }
        let mut toast = if level == Level::Reached {
            let mut t = Toast::new("error", format!("Resets {}", ul::format_reset(usage.resets_at))).title(format!("{} limit reached", model.name));
            t.action = Some(("Switch model".into(), "model.list".into()));
            t
        } else {
            let mut t = Toast::new("warning", format!("{} left · resets {}", ul::format_dollars(usage.left), ul::format_reset(usage.resets_at)))
                .title(format!("Running low on {}", model.name));
            t.action = Some(("View usage".into(), "usage.show".into()));
            t
        };
        toast.duration = 5000;
        self.toast(toast);
    }

    /// New limits arrived: the open dialogs follow them.
    pub(super) fn usage_limits_changed(&mut self) {
        self.refresh_usage_dialog();
        let hidden = self.usage_info().is_none();
        if let Some(sel) = self.dialogs.last_mut().filter(|d| d.purpose == "model").and_then(|d| d.sel()) {
            for a in sel.actions.iter_mut().filter(|a| a.id == "model.dialog.usage") {
                a.hidden = hidden;
            }
        }
    }

    /// `session.execution.succeeded` of the routed session: spend moved, so loaded limits reload.
    pub(super) fn usage_turn_ended(&mut self, sid: &str) {
        if self.session_id() == Some(sid) && self.usage_info().is_some() {
            self.usage_load(true);
        }
    }

    /// `/usage` (`DialogUsage`): reloads the limits and lists the models they apply to.
    pub fn open_usage(&mut self) {
        self.usage_load(true);
        let mut d = Dialog::text(PURPOSE, TextDialog::new("Usage", vec![]));
        d.size = Size::Medium;
        self.open_dialog(d);
        self.refresh_usage_dialog();
    }

    fn usage_rows(&self) -> Vec<(ModelInfo, ModelUsage, Level)> {
        let mut rows: Vec<(ModelInfo, ModelUsage, Level)> = self
            .models_ref()
            .iter()
            .filter_map(|m| {
                let u = self.model_usage(m)?;
                let level = ul::usage_level(&u, m);
                Some((m.clone(), u, level))
            })
            .collect();
        rows.sort_by(|a, b| crate::util::locale::compare(&a.0.name, &b.0.name));
        rows
    }

    fn dialog_width(&self) -> usize {
        (Size::Medium.width().min(self.size.0.saturating_sub(2)).max(10) as usize).saturating_sub(DIALOG_INSET * 2).max(1)
    }

    /// Rebuild the open usage dialog (or its per-model view) from the current limits.
    pub(super) fn refresh_usage_dialog(&mut self) {
        let Some(top) = self.dialogs.last() else { return };
        if top.purpose == MODEL_PURPOSE {
            let key = top.data.as_str().unwrap_or("").to_string();
            let td = self.usage_model_body(&key);
            if let Some(d) = self.dialogs.last_mut() {
                let scroll = match &d.body {
                    Body::Text(old) => old.scroll,
                    _ => 0,
                };
                let mut td = td;
                td.scroll = scroll;
                d.body = Body::Text(Box::new(td));
            }
            return;
        }
        if top.purpose != PURPOSE {
            return;
        }
        let t = self.theme().dialog().clone();
        let state = self.usage_state().map(|s| s.is_some());
        match state {
            Some(true) => {
                let rows = self.usage_rows();
                let opts: Vec<Opt> = rows
                    .iter()
                    .map(|(m, u, level)| {
                        let mut o = Opt::new(m.name.clone(), format!("{}/{}", m.provider_id, m.id));
                        o.footer = Some(format!("{} left", ul::format_dollars(u.left)));
                        if matches!(level, Level::Out | Level::Reached) {
                            o.footer_color = Some(t.text.feedback.error.base);
                        }
                        o
                    })
                    .collect();
                let Some(d) = self.dialogs.last_mut() else { return };
                match d.sel() {
                    Some(sel) => {
                        let current = sel.selected_value();
                        sel.options = opts;
                        if let Some(v) = current
                            && let Some(i) = sel.visible().iter().position(|i| sel.options[*i].value == v)
                        {
                            sel.selected = i;
                        }
                    }
                    None => {
                        let mut sel = Select::new("Usage", opts);
                        sel.placeholder = "Search models".into();
                        sel.footer_active_only = true;
                        sel.focus_current = false;
                        sel.empty = Some("No limits apply to your models. Paid models stop only if the workspace runs out of credits.".into());
                        d.body = Body::Select(Box::new(sel));
                    }
                }
            }
            other => {
                let text = if other == Some(false) { "Sign in to OpenCode Console with /connect to see your workspace's usage limits." } else { "Loading…" };
                let w = self.dialog_width();
                let lines = if other == Some(false) {
                    wrap(&[seg(text, fg(t.text.muted))], w, Wrap::Word).into_iter().map(SLine::new).collect()
                } else {
                    vec![SLine::new(vec![seg(text, fg(t.text.muted))])]
                };
                if let Some(d) = self.dialogs.last_mut() {
                    d.body = Body::Text(Box::new(TextDialog::new("Usage", lines)));
                }
            }
        }
    }

    /// Enter on a `/usage` row: `DialogUsageModel`.
    pub(super) fn open_usage_model(&mut self, key: &str) {
        let mut d = Dialog::text(MODEL_PURPOSE, TextDialog::new("Usage", vec![]));
        d.data = json!(key);
        self.open_dialog(d);
        self.refresh_usage_dialog();
    }

    /// `DialogUsageModel`: every limit on one model, the one setting what's left marked.
    fn usage_model_body(&self, key: &str) -> TextDialog {
        let t = self.theme().dialog().clone();
        let model = self.models_ref().iter().find(|m| format!("{}/{}", m.provider_id, m.id) == key).cloned();
        let name = model.as_ref().map(|m| m.name.clone()).unwrap_or_default();
        let w = self.dialog_width();
        let usage = model.as_ref().and_then(|m| self.model_usage(m));
        let mut lines = Vec::new();
        match (&model, &usage) {
            (Some(model), Some(u)) => {
                let short = matches!(ul::usage_level(u, model), Level::Out | Level::Reached);
                let workspace = self.usage_info().and_then(|i| i.workspace.name.clone());
                for l in wrap(&[Seg::new(format!("{} remaining", ul::format_dollars(u.left)), bold(fg(if short { t.text.feedback.error.base } else { t.text.base })))], w, Wrap::Word) {
                    lines.push(SLine::new(l));
                }
                for l in wrap(&[seg(format!("Resets {}", ul::format_reset(u.resets_at)), fg(t.text.muted))], w, Wrap::Word) {
                    lines.push(SLine::new(l));
                }
                lines.push(SLine::blank());
                lines.push(SLine::new(vec![seg("Limits", fg(t.text.muted))]));
                for (i, item) in u.limits.iter().enumerate() {
                    let mark = if i > 0 {
                        t.text.muted
                    } else if u.reached {
                        t.text.feedback.error.base
                    } else {
                        t.text.feedback.warning.base
                    };
                    let row = [
                        seg(format!("{} ", if i == 0 { "•" } else { "-" }), fg(mark)),
                        seg(format!("{} used of {}", ul::format_dollars(item.spent), ul::format_dollars(item.budget.limit)), fg(t.text.base)),
                    ];
                    for l in wrap(&row, w, Wrap::Word) {
                        lines.push(SLine::new(l));
                    }
                    for l in wrap(&[seg(ul::limit_scope(&item.budget, workspace.as_deref()), fg(t.text.muted))], w.saturating_sub(2).max(1), Wrap::Word) {
                        lines.push(SLine::new(l).prefixed(&[seg("  ", fg(t.text.muted))]));
                    }
                }
            }
            _ => {
                for l in wrap(&[seg(format!("No limits apply to {name}."), fg(t.text.muted))], w, Wrap::Word) {
                    lines.push(SLine::new(l));
                }
            }
        }
        let mut td = TextDialog::new(format!("Usage › {name}"), lines);
        td.title_view = Some(vec![seg("Usage › ", fg(t.text.muted)), seg(name, fg(t.text.base))]);
        td
    }
}
