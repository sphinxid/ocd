//! `/connect` (`component/dialog-integration.tsx`, `use-connected.tsx`): the integration list, the
//! account manager (switch, rename, delete), method choice, the sign-in forms, API keys, browser and
//! device OAuth, pasted authorization codes, external credential sources and login commands.
//!
//! Upstream chains these steps as promises around `dialog.replace`; here each step is a dialog with
//! purpose `integration` whose `data.flow` names a [`Live`] entry. A step whose dialog leaves the
//! stack (closed, cleared or replaced by anything else) is cleaned up like upstream's `onCleanup`:
//! a form stops, an unsettled sign-in attempt is cancelled.

use super::{App, Toast};
use crate::ui::choice::ChoiceField;
use crate::ui::dialog::{Action, Body, DIALOG_INSET, Dialog, Footer, FooterItem, Opt, Outcome, PromptDialog, Select, Size, TextDialog};
use crate::ui::form;
use crate::ui::text::{SLine, Seg, Wrap, fg, seg, wrap};
use crate::util::locale;
use serde_json::{Map, Value, json};

/// `INTEGRATION_PRIORITY`: the "Popular" integrations, in this order.
const PRIORITY: [&str; 6] = ["opencode", "opencode-go", "openai", "github-copilot", "anthropic", "google"];
/// Device-flow codes such as "R283-GOUEO"; when present they are copied instead of the URL.
const OAUTH_CODE: &str = r"[A-Z0-9]{4}-[A-Z0-9]{4,5}";
const PURPOSE: &str = "integration";

fn priority(id: &str) -> usize {
    PRIORITY.iter().position(|p| *p == id).unwrap_or(99)
}

fn s<'a>(v: &'a Value, k: &str) -> &'a str {
    v.get(k).and_then(|x| x.as_str()).unwrap_or("")
}

fn is_mcp(i: &Value) -> bool {
    i.pointer("/metadata/source").and_then(|x| x.as_str()) == Some("mcp")
}

fn arr(v: &Value, k: &str) -> Vec<Value> {
    v.get(k).and_then(|x| x.as_array()).cloned().unwrap_or_default()
}

/// `integrationOptions`: MCP integrations first, then the popular ones, then by name and ID.
pub fn integration_options(list: &[Value]) -> Vec<Value> {
    let mut out = list.to_vec();
    out.sort_by(|a, b| {
        is_mcp(b)
            .cmp(&is_mcp(a))
            .then(priority(s(a, "id")).cmp(&priority(s(b, "id"))))
            .then(locale::compare(s(a, "name"), s(b, "name")))
            .then(locale::compare(s(a, "id"), s(b, "id")))
    });
    out
}

/// `connectMethods`: every method but environment discovery, API keys last.
pub fn connect_methods(i: &Value) -> Vec<Value> {
    let mut out: Vec<Value> = arr(i, "methods").into_iter().filter(|m| s(m, "type") != "env").collect();
    out.sort_by_key(|m| s(m, "type") == "key");
    out
}

/// `credentialConnections`: the saved (removable) connections.
pub fn credential_connections(i: &Value) -> Vec<Value> {
    arr(i, "connections").into_iter().filter(|c| s(c, "type") == "credential").collect()
}

/// `connectionSummary`: a count keeps several long labels from squeezing the name off the row.
pub fn connection_summary(i: &Value) -> String {
    let connections = arr(i, "connections");
    if connections.len() > 1 {
        return format!("{} connections", connections.len());
    }
    match connections.first() {
        None => String::new(),
        Some(c) if s(c, "type") == "credential" => s(c, "label").to_string(),
        Some(c) => format!("${}", s(c, "name")),
    }
}

/// `hasConnectedProvider`: some integration has a connection (a credential or an environment variable).
pub fn has_connected_provider(list: &[Value]) -> bool {
    list.iter().any(|i| !arr(i, "connections").is_empty())
}

fn status_of(c: &Value) -> Option<&Value> {
    c.get("status").filter(|x| !x.is_null())
}

/// `active(field, answer)`: a `when` condition needs its key answered.
fn active(f: &Value, answer: &Map<String, Value>) -> bool {
    if form::kind(f) == "external" {
        return true;
    }
    let Some(when) = f.get("when").and_then(|w| w.as_array()) else { return true };
    when.iter().all(|w| {
        let Some(value) = answer.get(s(w, "key")) else { return false };
        let target = w.get("value").cloned().unwrap_or(Value::Null);
        let hit = match value.as_array() {
            Some(items) => {
                let text = match &target {
                    Value::String(t) => t.clone(),
                    other => other.to_string(),
                };
                items.iter().any(|x| x.as_str() == Some(text.as_str()))
            }
            None => *value == target,
        };
        if s(w, "op") == "eq" { hit } else { !hit }
    })
}

/// JS `String(value)`.
fn js_string(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Number(n) => n.as_f64().map(locale::js_number).unwrap_or_else(|| n.to_string()),
        other => other.to_string(),
    }
}

/// JS `Number(text)` for a trimmed input: empty is 0, anything unparsable is NaN (`None`).
fn js_number(text: &str) -> Option<f64> {
    if text.is_empty() {
        return Some(0.0);
    }
    match text {
        "Infinity" | "+Infinity" => return Some(f64::INFINITY),
        "-Infinity" => return Some(f64::NEG_INFINITY),
        _ => {}
    }
    if text.chars().any(|c| c.is_ascii_alphabetic() && c != 'e' && c != 'E') {
        return None;
    }
    text.parse::<f64>().ok()
}

/// Where a finished connection leads.
#[derive(Clone, Debug, Default)]
pub struct Ctx {
    pub dir: String,
    /// `onConnected` opens the model list for the new provider.
    pub then_models: bool,
}

#[derive(Clone, Debug)]
enum After {
    Key,
    External,
    OAuth,
}

#[derive(Clone, Debug)]
enum FieldState {
    Plain,
    Multi(Vec<String>),
    /// The multiselect's "Type your own answer" prompt.
    MultiCustom(Vec<String>),
    External(bool),
}

#[derive(Clone, Debug)]
struct FormRun {
    title: String,
    fields: Vec<Value>,
    index: usize,
    answer: Map<String, Value>,
    integration: Value,
    method: Value,
    after: After,
    state: FieldState,
}

#[derive(Clone, Debug)]
enum Step {
    List { only: Option<String>, auto: bool },
    Manage { integration: Value, methods: Vec<Value>, deleting: Option<String>, selected: Option<String> },
    Methods,
    Rename { integration: Value, methods: Vec<Value>, credential: String },
    Form(Box<FormRun>),
    Key { integration: Value, answer: Option<Value> },
    Waiting,
    OAuthAuto { integration: Value, attempt: Value, settled: bool },
    OAuthCode { integration: Value, attempt: Value, settled: bool },
    CommandStarting,
    CommandPending { integration: Value, attempt: Value, settled: bool },
}

#[derive(Clone, Debug)]
struct Live {
    token: u64,
    ctx: Ctx,
    step: Step,
}

/// The live integration dialogs.
#[derive(Clone, Debug, Default)]
pub struct Flows {
    live: Vec<Live>,
    next: u64,
}

impl App {
    // ---- entry points --------------------------------------------------------

    /// `DialogIntegration`: `/connect`, the model list's "Connect an integration" (both continue to
    /// the model list), the prompt's "Connect an integration to send prompts", and an MCP server that
    /// needs a sign-in (`integrationID` with `autoConnect`).
    pub fn open_integrations(&mut self, then_models: bool, only: Option<String>, auto: bool) {
        let ctx = Ctx { dir: self.location_dir(), then_models };
        let sel = self.integration_list_select(&ctx, only.as_deref());
        self.integration_show(ctx.clone(), Step::List { only: only.clone(), auto }, Dialog::select(PURPOSE, sel, Size::Medium));
        if auto {
            self.integration_auto_connect();
        }
    }

    /// `DialogMcp`: a server that needs a sign-in through an integration enters its sign-in.
    pub fn mcp_sign_in(&mut self, name: &str) -> bool {
        let server = self.loc().and_then(|l| l.mcp.clone()).unwrap_or_default().into_iter().find(|s| s.name == name);
        let Some(id) = server.filter(|s| s.status.status == "needs_auth").and_then(|s| s.integration_id) else { return false };
        self.open_integrations(false, Some(id), true);
        true
    }

    fn integrations_at(&self, dir: &str) -> Vec<Value> {
        self.store.locations.get(dir).and_then(|l| l.integrations.clone()).unwrap_or_default()
    }

    fn integration_current(&self, ctx: &Ctx, fallback: &Value) -> Value {
        let id = s(fallback, "id");
        self.integrations_at(&ctx.dir).into_iter().find(|i| s(i, "id") == id).unwrap_or_else(|| fallback.clone())
    }

    // ---- flow plumbing ---------------------------------------------------------

    fn integration_show(&mut self, ctx: Ctx, step: Step, mut d: Dialog) -> u64 {
        let token = self.integration.next;
        self.integration.next += 1;
        d.purpose = PURPOSE.into();
        d.data = json!({ "flow": token });
        self.integration.live.push(Live { token, ctx, step });
        self.open_dialog(d);
        token
    }

    fn integration_shown(&self, token: u64) -> bool {
        self.dialogs.iter().any(|d| d.purpose == PURPOSE && d.data.get("flow").and_then(|f| f.as_u64()) == Some(token))
    }

    fn integration_top(&self) -> Option<u64> {
        self.dialogs.last().filter(|d| d.purpose == PURPOSE).and_then(|d| d.data.get("flow").and_then(|f| f.as_u64()))
    }

    /// The step of the top integration dialog (tests).
    #[cfg(test)]
    pub fn integration_top_step(&self) -> Option<&'static str> {
        let token = self.integration_top()?;
        self.integration.live.iter().find(|l| l.token == token).map(|l| match l.step {
            Step::List { .. } => "list",
            Step::Manage { .. } => "manage",
            Step::Methods => "methods",
            Step::Rename { .. } => "rename",
            Step::Form(_) => "form",
            Step::Key { .. } => "key",
            Step::Waiting => "waiting",
            Step::OAuthAuto { .. } => "auto",
            Step::OAuthCode { .. } => "code",
            Step::CommandStarting => "command-starting",
            Step::CommandPending { .. } => "command",
        })
    }

    fn live(&mut self, token: u64) -> Option<&mut Live> {
        self.integration.live.iter_mut().find(|l| l.token == token)
    }

    fn top_dialog(&mut self, token: u64) -> Option<&mut Dialog> {
        self.dialogs.iter_mut().rev().find(|d| d.purpose == PURPOSE && d.data.get("flow").and_then(|f| f.as_u64()) == Some(token))
    }

    /// `onCleanup` of the steps whose dialog left the stack: cancel unsettled attempts.
    pub fn integration_reconcile(&mut self) {
        if self.integration.live.is_empty() {
            return;
        }
        let shown: Vec<u64> = self.dialogs.iter().filter(|d| d.purpose == PURPOSE).filter_map(|d| d.data.get("flow").and_then(|f| f.as_u64())).collect();
        let (keep, gone): (Vec<Live>, Vec<Live>) = std::mem::take(&mut self.integration.live).into_iter().partition(|l| shown.contains(&l.token));
        self.integration.live = keep;
        for l in gone {
            let dir = l.ctx.dir.clone();
            let c = self.client.clone();
            match l.step {
                Step::OAuthAuto { integration, attempt, settled: false } | Step::OAuthCode { integration, attempt, settled: false } => {
                    let (id, attempt) = (s(&integration, "id").to_string(), s(&attempt, "attemptID").to_string());
                    self.spawn(async move { c.integration_oauth_cancel(&id, &attempt, Some(&dir)).await }, |_, _| {});
                }
                Step::CommandPending { integration, attempt, settled: false } => {
                    let (id, attempt) = (s(&integration, "id").to_string(), s(&attempt, "attemptID").to_string());
                    self.spawn(async move { c.integration_command_cancel(&id, &attempt, Some(&dir)).await }, |_, _| {});
                }
                _ => {}
            }
        }
    }

    /// The location's integrations changed: refresh an open list or account manager.
    pub fn integration_catalog_changed(&mut self, dir: &str) {
        let Some(token) = self.integration_top() else { return };
        let Some(live) = self.integration.live.iter().find(|l| l.token == token).cloned() else { return };
        if live.ctx.dir != dir {
            return;
        }
        match &live.step {
            Step::List { only, auto } => {
                if *auto && self.integration_auto_connect() {
                    return;
                }
                let fresh = self.integration_list_select(&live.ctx, only.as_deref());
                self.integration_refresh_select(token, fresh);
            }
            Step::Manage { integration, methods, deleting, selected } => {
                let fresh = self.manage_select(&live.ctx, integration, methods, deleting.as_deref(), selected.as_deref());
                self.integration_refresh_select(token, fresh);
            }
            _ => {}
        }
    }

    /// Swap a select's options in place, keeping the filter and the selected value (`preserveSelection`).
    fn integration_refresh_select(&mut self, token: u64, fresh: Select) {
        let Some(sel) = self.top_dialog(token).and_then(|d| d.sel()) else { return };
        let value = sel.selected_value();
        let (filter, scroll, index) = (sel.filter.clone(), sel.scroll, sel.selected);
        *sel = fresh;
        sel.filter = filter;
        sel.scroll = scroll;
        let visible = sel.visible();
        sel.selected = value
            .and_then(|v| visible.iter().position(|i| sel.options[*i].value == v))
            .unwrap_or_else(|| index.min(visible.len().saturating_sub(1)));
        self.dirty = true;
    }

    /// `autoConnect`: go straight to the only listed integration's accounts or methods.
    fn integration_auto_connect(&mut self) -> bool {
        let Some(token) = self.integration_top() else { return false };
        let Some(Live { ctx, step: Step::List { only, auto: true }, .. }) = self.integration.live.iter().find(|l| l.token == token).cloned() else { return false };
        let list: Vec<Value> = integration_options(&self.integrations_at(&ctx.dir)).into_iter().filter(|i| only.as_deref().is_none_or(|o| s(i, "id") == o)).collect();
        let Some(integration) = list.into_iter().next() else { return false };
        let methods = connect_methods(&integration);
        if !credential_connections(&integration).is_empty() {
            self.manage_connections(ctx, integration, methods);
        } else {
            self.select_method(ctx, integration, methods);
        }
        true
    }

    // ---- the integration list ----------------------------------------------------

    fn integration_list_select(&self, ctx: &Ctx, only: Option<&str>) -> Select {
        let t = self.theme().dialog().clone();
        let options: Vec<Opt> = integration_options(&self.integrations_at(&ctx.dir))
            .into_iter()
            .filter(|i| only.is_none_or(|o| s(i, "id") == o))
            .map(|i| {
                let methods = connect_methods(&i);
                let credentials = credential_connections(&i);
                let id = s(&i, "id").to_string();
                let category = if is_mcp(&i) {
                    "MCP"
                } else if PRIORITY.contains(&id.as_str()) {
                    "Popular"
                } else {
                    "Services"
                };
                let connections = arr(&i, "connections");
                let status = connections.first().and_then(status_of).is_some();
                let mut o = Opt::new(s(&i, "name"), id).cat(category);
                if methods.is_empty() {
                    o.description = Some("Environment only".into());
                }
                if status {
                    o.footer = Some("Sign in required →".into());
                    o.footer_color = Some(t.text.feedback.warning.base);
                } else {
                    o.footer = Some(connection_summary(&i)).filter(|f| !f.is_empty());
                }
                o.disabled = methods.is_empty() && credentials.is_empty();
                if !connections.is_empty() && !status {
                    o.gutter = Some(Seg::new("✓", fg(t.text.feedback.success.base)));
                }
                o
            })
            .collect();
        let mut sel = Select::new("Connect an integration", options);
        sel.empty = Some("No integrations available".into());
        sel.no_match = Some("No integrations found".into());
        sel
    }

    fn integration_list_selected(&mut self, ctx: Ctx, id: &str) {
        let Some(integration) = self.integrations_at(&ctx.dir).into_iter().find(|i| s(i, "id") == id) else { return };
        let methods = connect_methods(&integration);
        if !credential_connections(&integration).is_empty() {
            self.manage_connections(ctx, integration, methods);
        } else {
            self.select_method(ctx, integration, methods);
        }
    }

    // ---- the account manager -------------------------------------------------------

    fn manage_select(&self, ctx: &Ctx, integration: &Value, methods: &[Value], deleting: Option<&str>, selected: Option<&str>) -> Select {
        let t = self.theme().dialog().clone();
        let current = self.integration_current(ctx, integration);
        let mut credentials = credential_connections(&current);
        let active = credentials.first().map(|c| s(c, "id").to_string());
        credentials.sort_by(|a, b| locale::compare(s(a, "label"), s(b, "label")).then(locale::compare(s(a, "id"), s(b, "id"))));
        let mut options = Vec::new();
        if !methods.is_empty() {
            options.push(Opt::new("Add account", "add"));
        }
        for c in &credentials {
            let id = s(c, "id");
            let confirming = deleting == Some(id);
            let title = if confirming { format!("Press {} again to confirm", self.keybinds.shortcut("dialog.integration.delete")) } else { s(c, "label").to_string() };
            let mut o = Opt::new(title, id).cat("Connected accounts");
            if status_of(c).is_some() {
                o.footer = Some("Sign in required →".into());
                o.footer_color = Some(t.text.feedback.warning.base);
            }
            if confirming {
                o.bg = Some(t.background.action.destructive.focused);
                o.fg = Some(t.text.action.destructive.focused);
            }
            options.push(o);
        }
        let mut sel = Select::new(s(&current, "name"), options);
        sel.current = active;
        sel.focus_current = false;
        let account = selected.is_some_and(|v| v != "add");
        let mut rename = Action::new("dialog.integration.rename", "rename").hidden(!account);
        rename.disabled = !account;
        let mut delete = Action::new("dialog.integration.delete", "delete").hidden(!account);
        delete.disabled = !account;
        sel.actions = vec![rename, delete];
        sel
    }

    fn manage_connections(&mut self, ctx: Ctx, integration: Value, methods: Vec<Value>) {
        let selected = if !methods.is_empty() { Some("add".to_string()) } else { credential_connections(&integration).first().map(|c| s(c, "id").to_string()) };
        let sel = self.manage_select(&ctx, &integration, &methods, None, selected.as_deref());
        self.integration_show(ctx, Step::Manage { integration, methods, deleting: None, selected }, Dialog::select(PURPOSE, sel, Size::Medium));
    }

    fn manage_rebuild(&mut self, token: u64) {
        let Some(Live { ctx, step: Step::Manage { integration, methods, deleting, selected }, .. }) = self.live(token).cloned() else { return };
        let fresh = self.manage_select(&ctx, &integration, &methods, deleting.as_deref(), selected.as_deref());
        self.integration_refresh_select(token, fresh);
    }

    fn manage_chosen(&mut self, ctx: Ctx, integration: Value, methods: Vec<Value>, value: &str) {
        let current = self.integration_current(&ctx, &integration);
        if value == "add" {
            self.select_method(ctx, current, methods);
            return;
        }
        let credentials = credential_connections(&current);
        let Some(c) = credentials.iter().find(|c| s(c, "id") == value) else { return };
        if let Some(status) = status_of(c) {
            if let Some(url) = status.get("url").and_then(|u| u.as_str()) {
                if let Err(e) = open_url(url) {
                    self.toast(Toast::new("error", e));
                }
                return;
            }
            self.select_method(ctx, current, methods);
            return;
        }
        if credentials.first().map(|c| s(c, "id")) == Some(value) {
            return;
        }
        let c = self.client.clone();
        let id = value.to_string();
        self.spawn(async move { c.credential_activate(&id).await }, |app, r| {
            if let Err(e) = r {
                app.toast_error(&e);
            }
        });
    }

    fn manage_action(&mut self, token: u64, action: &str, value: Option<String>) {
        let Some(Live { ctx, step: Step::Manage { integration, methods, deleting, .. }, .. }) = self.live(token).cloned() else { return };
        let Some(value) = value.filter(|v| v != "add") else { return };
        let current = self.integration_current(&ctx, &integration);
        match action {
            "dialog.integration.rename" => {
                let label = credential_connections(&current).iter().find(|c| s(c, "id") == value).map(|c| s(c, "label").to_string()).unwrap_or_default();
                let d = Dialog::new(PURPOSE, Body::Prompt(PromptDialog::new("Rename account", "Account name", &label)));
                self.integration_show(ctx, Step::Rename { integration, methods, credential: value }, d);
            }
            "dialog.integration.delete" => {
                if deleting.as_deref() != Some(value.as_str()) {
                    if let Some(Live { step: Step::Manage { deleting, .. }, .. }) = self.live(token) {
                        *deleting = Some(value);
                    }
                    self.manage_rebuild(token);
                    return;
                }
                let last = credential_connections(&current).len() == 1;
                let name = s(&integration, "name").to_string();
                let c = self.client.clone();
                self.spawn(async move { c.credential_remove(&value).await }, move |app, r| {
                    if let Some(Live { step: Step::Manage { deleting, .. }, .. }) = app.live(token) {
                        *deleting = None;
                    }
                    app.manage_rebuild(token);
                    match r {
                        Ok(()) => {
                            if last {
                                app.toast(Toast::new("success", format!("Disconnected {name}")));
                                app.dialogs.clear();
                            }
                        }
                        Err(e) => app.toast_error(&e),
                    }
                    app.dirty = true;
                });
            }
            _ => {}
        }
    }

    fn rename_submit(&mut self, token: u64, text: &str) {
        let Some(Live { ctx, step: Step::Rename { integration, methods, credential }, .. }) = self.live(token).cloned() else { return };
        let label = text.trim().to_string();
        if label.is_empty() {
            return;
        }
        let c = self.client.clone();
        self.spawn(async move { c.credential_update(&credential, &label).await }, move |app, r| match r {
            Ok(()) => app.manage_connections(ctx, integration, methods),
            Err(e) => app.toast_error(&e),
        });
    }

    // ---- methods ---------------------------------------------------------------------

    fn select_method(&mut self, ctx: Ctx, integration: Value, methods: Vec<Value>) {
        if methods.len() == 1 {
            self.open_method(ctx, integration, methods[0].clone());
            return;
        }
        let options = methods
            .iter()
            .map(|m| {
                if s(m, "type") == "key" {
                    Opt::new(m.get("label").and_then(|l| l.as_str()).unwrap_or("API key"), "key")
                } else {
                    Opt::new(s(m, "label"), s(m, "id"))
                }
            })
            .collect();
        let sel = Select::new(format!("Connect {}", s(&integration, "name")), options);
        let token = self.integration_show(ctx, Step::Methods, Dialog::select(PURPOSE, sel, Size::Medium));
        if let Some(d) = self.top_dialog(token) {
            d.data["integration"] = integration;
            d.data["methods"] = Value::Array(methods);
        }
    }

    fn open_method(&mut self, ctx: Ctx, integration: Value, method: Value) {
        let title = match s(&method, "type") {
            "key" => method.get("label").and_then(|l| l.as_str()).map(|l| l.to_string()).unwrap_or_else(|| format!("Connect {}", s(&integration, "name"))),
            _ => s(&method, "label").to_string(),
        };
        let after = match s(&method, "type") {
            "key" => After::Key,
            "command" => {
                self.command_start(ctx, integration, method);
                return;
            }
            "external" => After::External,
            _ => After::OAuth,
        };
        match method.get("form").and_then(|f| f.as_array()).cloned() {
            Some(fields) => {
                let run = FormRun { title, fields, index: 0, answer: Map::new(), integration, method, after, state: FieldState::Plain };
                self.iform_advance(ctx, run);
            }
            None => self.iform_done(ctx, integration, method, after, None),
        }
    }

    fn iform_done(&mut self, ctx: Ctx, integration: Value, method: Value, after: After, answer: Option<Value>) {
        match after {
            After::Key => self.key_prompt(ctx, integration, method, answer),
            After::External => self.external_start(ctx, integration, method, answer),
            After::OAuth => self.oauth_start(ctx, integration, method, answer),
        }
    }

    // ---- forms (`formAnswer`) -------------------------------------------------------------

    fn iform_advance(&mut self, ctx: Ctx, mut run: FormRun) {
        while let Some(f) = run.fields.get(run.index).cloned() {
            if !active(&f, &run.answer) {
                run.index += 1;
                continue;
            }
            if form::kind(&f) != "external" && f.get("hidden").and_then(|h| h.as_bool()) == Some(true) {
                if let Some(d) = f.get("default") {
                    run.answer.insert(form::key(&f), d.clone());
                }
                run.index += 1;
                continue;
            }
            run.state = match form::kind(&f) {
                "external" => FieldState::External(false),
                "multiselect" => FieldState::Multi(f.get("default").and_then(|d| d.as_array()).map(|a| a.iter().filter_map(|x| x.as_str().map(|s| s.to_string())).collect()).unwrap_or_default()),
                _ => FieldState::Plain,
            };
            self.iform_show(ctx, run);
            return;
        }
        let FormRun { integration, method, after, answer, .. } = run;
        self.iform_done(ctx, integration, method, after, Some(Value::Object(answer)));
    }

    fn iform_show(&mut self, ctx: Ctx, run: FormRun) {
        let f = run.fields[run.index].clone();
        let label = form::label(&f);
        let title = if label.is_empty() { run.title.clone() } else { label };
        let d = match (&run.state, form::kind(&f)) {
            (FieldState::External(opened), _) => {
                let mut open = Opt::new(if *opened { "Open link again" } else { "Open link" }, "open");
                open.description = Some(s(&f, "url").to_string()).filter(|u| !u.is_empty());
                let mut done = Opt::new("I finished", "done");
                done.description = f.get("description").and_then(|d| d.as_str()).map(|d| d.to_string());
                done.disabled = !*opened;
                Dialog::select(PURPOSE, Select::new(title, vec![open, done]), Size::Medium)
            }
            (FieldState::Multi(selected), _) => {
                let invalid = form::validate(&f, Some(&json!(selected)));
                let max = f.get("maxItems").and_then(|m| m.as_f64());
                let mut options: Vec<Opt> = form::rows(&f)
                    .into_iter()
                    .map(|(v, label, desc)| {
                        let value = v.as_str().unwrap_or("").to_string();
                        let on = selected.contains(&value);
                        let mut o = Opt::new(format!("[{}] {label}", if on { "x" } else { " " }), format!("o:{value}"));
                        o.description = desc;
                        o.disabled = !on && max.is_some_and(|m| selected.len() as f64 >= m);
                        o
                    })
                    .collect();
                if form::custom(&f) {
                    options.push(Opt::new("Type your own answer", "custom"));
                }
                let mut cont = Opt::new("Continue", "submit");
                cont.description = invalid.clone();
                cont.disabled = invalid.is_some();
                options.push(cont);
                Dialog::select(PURPOSE, Select::new(title, options), Size::Medium)
            }
            (FieldState::MultiCustom(_), _) => Dialog::new(PURPOSE, Body::Prompt(PromptDialog::new(&title, "Type your own answer", ""))),
            (FieldState::Plain, "boolean") | (FieldState::Plain, "string") if form::kind(&f) == "boolean" || form::options(&f).is_some() => {
                if form::kind(&f) == "string" && f.get("custom").and_then(|c| c.as_bool()) == Some(true) {
                    Dialog::new(PURPOSE, Body::Choice(Box::new(ChoiceField::new(f.clone()))))
                } else {
                    let rows: Vec<(Value, String, Option<String>)> = if form::kind(&f) == "boolean" {
                        if f.get("default") == Some(&Value::Bool(false)) {
                            vec![(json!(false), "No".into(), None), (json!(true), "Yes".into(), None)]
                        } else {
                            vec![(json!(true), "Yes".into(), None), (json!(false), "No".into(), None)]
                        }
                    } else {
                        form::rows(&f)
                    };
                    let mut options: Vec<Opt> = rows
                        .into_iter()
                        .map(|(v, label, desc)| {
                            let mut o = Opt::new(label, format!("j:{v}"));
                            o.description = desc;
                            o
                        })
                        .collect();
                    if f.get("required").and_then(|r| r.as_bool()) != Some(true) {
                        options.push(Opt::new("Skip", "skip"));
                    }
                    let mut sel = Select::new(title, options);
                    if form::kind(&f) == "string" {
                        sel.current = f.get("default").map(|d| format!("j:{d}"));
                    }
                    Dialog::select(PURPOSE, sel, Size::Medium)
                }
            }
            _ => {
                let placeholder = if form::kind(&f) == "string" { s(&f, "placeholder").to_string() } else { String::new() };
                let initial = f.get("default").map(js_string).unwrap_or_default();
                let mut p = PromptDialog::new(&title, &placeholder, &initial);
                p.description = self.text_field_description(&f, None);
                // The description box is there even when empty, so it keeps a row.
                p.description_gap = true;
                Dialog::new(PURPOSE, Body::Prompt(p))
            }
        };
        self.integration_show(ctx, Step::Form(Box::new(run)), d);
    }

    fn text_field_description(&self, f: &Value, error: Option<&str>) -> Vec<(String, crate::theme::Rgba)> {
        let t = self.theme().dialog();
        let mut out = Vec::new();
        if let Some(d) = f.get("description").and_then(|d| d.as_str()) {
            out.push((d.to_string(), t.text.muted));
        }
        if let Some(e) = error {
            if !out.is_empty() {
                out.push((String::new(), t.text.muted));
            }
            out.push((e.to_string(), t.text.feedback.error.base));
        }
        self.prewrap(out)
    }

    /// A field answered (`None`: skipped); go to the next one.
    fn iform_answer(&mut self, ctx: Ctx, mut run: FormRun, value: Option<Value>) {
        if let Some(v) = value {
            run.answer.insert(form::key(&run.fields[run.index]), v);
        }
        run.index += 1;
        run.state = FieldState::Plain;
        self.iform_advance(ctx, run);
    }

    fn iform_select(&mut self, token: u64, value: &str) {
        let Some(Live { ctx, step: Step::Form(run), .. }) = self.live(token).cloned() else { return };
        let mut run = *run;
        let f = run.fields[run.index].clone();
        match run.state.clone() {
            FieldState::External(opened) => {
                if value == "done" {
                    self.iform_answer(ctx, run, Some(Value::Bool(true)));
                    return;
                }
                // `Opening link…` while the browser starts, then the choice again.
                let ok = open_url(s(&f, "url")).is_ok();
                run.state = FieldState::External(opened || ok);
                self.iform_show(ctx, run);
            }
            FieldState::Multi(mut selected) => match value {
                "submit" => self.iform_answer(ctx, run, Some(json!(selected))),
                "custom" => {
                    run.state = FieldState::MultiCustom(selected);
                    self.iform_show(ctx, run);
                }
                other => {
                    let item = other.strip_prefix("o:").unwrap_or(other).to_string();
                    match selected.iter().position(|x| *x == item) {
                        Some(i) => {
                            selected.remove(i);
                        }
                        None => selected.push(item),
                    }
                    run.state = FieldState::Multi(selected);
                    self.iform_show(ctx, run);
                }
            },
            _ => {
                let answer = if value == "skip" { None } else { value.strip_prefix("j:").and_then(|j| serde_json::from_str(j).ok()) };
                self.iform_answer(ctx, run, answer);
            }
        }
    }

    fn iform_submit(&mut self, token: u64, text: &str) {
        let Some(Live { ctx, step: Step::Form(run), .. }) = self.live(token).cloned() else { return };
        let mut run = *run;
        let f = run.fields[run.index].clone();
        match run.state.clone() {
            FieldState::MultiCustom(mut selected) => {
                if text.is_empty() {
                    return;
                }
                if !selected.iter().any(|x| x == text) {
                    selected.push(text.to_string());
                }
                run.state = FieldState::Multi(selected);
                self.iform_show(ctx, run);
            }
            _ if matches!(self.dialogs.last().map(|d| &d.body), Some(Body::Choice(_))) => {
                let value: Option<Value> = serde_json::from_str(text).unwrap_or(None);
                self.iform_answer(ctx, run, value);
            }
            _ => {
                let input = text.trim();
                let required = f.get("required").and_then(|r| r.as_bool()) == Some(true);
                let value = if input.is_empty() && !required {
                    None
                } else if form::kind(&f) == "string" {
                    Some(Value::String(input.to_string()))
                } else {
                    Some(js_number(input).and_then(serde_json::Number::from_f64).map(Value::Number).unwrap_or(Value::Null))
                };
                if let Some(invalid) = form::validate(&f, value.as_ref()) {
                    let desc = self.text_field_description(&f, Some(&invalid));
                    if let Some(Body::Prompt(p)) = self.top_dialog(token).map(|d| &mut d.body) {
                        p.description = desc;
                    }
                    return;
                }
                self.iform_answer(ctx, run, value);
            }
        }
    }

    // ---- API keys ---------------------------------------------------------------------------

    fn key_prompt(&mut self, ctx: Ctx, integration: Value, method: Value, answer: Option<Value>) {
        let title = method.get("label").and_then(|l| l.as_str()).map(|l| l.to_string()).unwrap_or_else(|| format!("Connect {}", s(&integration, "name")));
        let d = Dialog::new(PURPOSE, Body::Prompt(PromptDialog::new(&title, "API key", "")));
        self.integration_show(ctx, Step::Key { integration, answer }, d);
    }

    fn key_submit(&mut self, token: u64, key: &str) {
        let Some(Live { ctx, step: Step::Key { integration, answer }, .. }) = self.live(token).cloned() else { return };
        if key.is_empty() {
            return;
        }
        let c = self.client.clone();
        let (id, key, dir) = (s(&integration, "id").to_string(), key.to_string(), ctx.dir.clone());
        self.spawn(async move { c.integration_connect_key(&id, &key, answer, Some(&dir)).await }, move |app, r| match r {
            Ok(()) => app.integration_connected(ctx, integration, Some(token)),
            Err(e) => app.prompt_error(token, &crate::api::error_message(&e)),
        });
    }

    /// The red line above a prompt's input (`setError`).
    fn prompt_error(&mut self, token: u64, message: &str) {
        let t = self.theme().dialog().clone();
        let step = self.live(token).map(|l| l.step.clone());
        let description = match step {
            Some(Step::OAuthCode { attempt, .. }) => self.code_description(&attempt, Some(message)),
            _ => self.prewrap(vec![(message.to_string(), t.text.feedback.error.base)]),
        };
        if let Some(Body::Prompt(p)) = self.top_dialog(token).map(|d| &mut d.body) {
            p.description = description;
        }
        self.dirty = true;
    }

    // ---- external sources and OAuth ---------------------------------------------------------

    /// `OAuthView` in the legacy layout: the link, the instructions, the status line and the
    /// `open o  copy c` footer (empty before an attempt exists).
    fn oauth_view(&self, title: &str, attempt: Option<&Value>, message: &str) -> TextDialog {
        let t = self.theme().dialog().clone();
        let inner = dialog_inner(self.size.0, Size::Medium);
        let mut lines: Vec<SLine> = Vec::new();
        if let Some(a) = attempt {
            lines.extend(wrapped(s(a, "url"), inner, fg(t.markdown.link)));
            let instructions = s(a, "instructions");
            if !instructions.is_empty() {
                lines.push(SLine::blank());
                lines.extend(wrapped(instructions, inner, fg(t.text.muted)));
            }
            lines.push(SLine::blank());
        }
        lines.extend(wrapped(message, inner, fg(t.text.muted)));
        let mut td = TextDialog::new(title, lines);
        if attempt.is_some() {
            td.footer = Some(Footer { secondary: vec![FooterItem::new("open", "o"), FooterItem::new("copy", "c")], ..Default::default() });
        } else {
            td.empty_footer_gap = true;
        }
        td
    }

    fn external_start(&mut self, ctx: Ctx, integration: Value, method: Value, answer: Option<Value>) {
        let d = Dialog::text(PURPOSE, self.oauth_view(s(&method, "label"), None, "Connecting…"));
        self.integration_show(ctx.clone(), Step::Waiting, d);
        let c = self.client.clone();
        let (id, mid, dir) = (s(&integration, "id").to_string(), s(&method, "id").to_string(), ctx.dir.clone());
        self.spawn(async move { c.integration_connect_external(&id, &mid, answer, Some(&dir)).await }, move |app, r| match r {
            Ok(()) => app.integration_connected(ctx, integration, None),
            Err(e) => {
                app.toast(Toast::new("error", crate::api::error_message(&e)));
                app.dialogs.clear();
            }
        });
    }

    fn oauth_start(&mut self, ctx: Ctx, integration: Value, method: Value, answer: Option<Value>) {
        let title = s(&method, "label").to_string();
        let d = Dialog::text(PURPOSE, self.oauth_view(&title, None, "Starting authorization…"));
        self.integration_show(ctx.clone(), Step::Waiting, d);
        let c = self.client.clone();
        let (id, mid, dir) = (s(&integration, "id").to_string(), s(&method, "id").to_string(), ctx.dir.clone());
        self.spawn(async move { c.integration_oauth_connect(&id, &mid, answer, Some(&dir)).await }, move |app, r| match r {
            Ok(attempt) => {
                if s(&attempt, "mode") == "code" {
                    app.oauth_code(ctx, integration, title, attempt);
                } else {
                    app.oauth_auto(ctx, integration, title, attempt);
                }
            }
            Err(e) => {
                app.toast(Toast::new("error", crate::api::error_message(&e)));
                app.dialogs.clear();
            }
        });
    }

    fn oauth_auto(&mut self, ctx: Ctx, integration: Value, title: String, attempt: Value) {
        let d = Dialog::text(PURPOSE, self.oauth_view(&title, Some(&attempt), "Waiting for authorization…"));
        let token = self.integration_show(ctx, Step::OAuthAuto { integration, attempt, settled: false }, d);
        self.oauth_poll(token, false);
    }

    fn oauth_poll(&mut self, token: u64, wait: bool) {
        let Some(Live { ctx, step: Step::OAuthAuto { integration, attempt, .. }, .. }) = self.live(token).cloned() else { return };
        let c = self.client.clone();
        let (id, aid, dir) = (s(&integration, "id").to_string(), s(&attempt, "attemptID").to_string(), ctx.dir.clone());
        self.spawn(
            async move {
                if wait {
                    tokio::time::sleep(std::time::Duration::from_millis(500)).await;
                }
                c.integration_oauth_status(&id, &aid, Some(&dir)).await
            },
            move |app, r| {
                if !app.integration_shown(token) {
                    return;
                }
                let status = match r {
                    Ok(v) => v,
                    Err(e) => {
                        app.settle(token);
                        app.toast(Toast::new("error", crate::api::error_message(&e)));
                        app.dialogs.clear();
                        return;
                    }
                };
                match s(&status, "status") {
                    "pending" => app.oauth_poll(token, true),
                    "complete" => {
                        app.settle(token);
                        app.integration_connected(ctx, integration, None);
                    }
                    other => {
                        app.settle(token);
                        let message = if other == "failed" { s(&status, "message").to_string() } else { "Authorization expired".to_string() };
                        app.toast(Toast::new("error", message));
                        app.dialogs.clear();
                    }
                }
            },
        );
    }

    fn settle(&mut self, token: u64) {
        if let Some(l) = self.live(token) {
            match &mut l.step {
                Step::OAuthAuto { settled, .. } | Step::OAuthCode { settled, .. } | Step::CommandPending { settled, .. } => *settled = true,
                _ => {}
            }
        }
    }

    /// `o` opens the authorization URL, `c` copies the device code (or the URL).
    fn oauth_key(&mut self, token: u64, key: &str) {
        let Some(Live { step: Step::OAuthAuto { attempt, .. }, .. }) = self.live(token).cloned() else { return };
        let url = s(&attempt, "url");
        match key {
            "o" => {
                if open_url(url).is_err() {
                    self.toast(Toast::new("error", "Could not open the browser. Copy the URL and continue manually."));
                }
            }
            "c" => {
                let re = regex::Regex::new(OAUTH_CODE).unwrap();
                let value = re.find(s(&attempt, "instructions")).map(|m| m.as_str().to_string()).unwrap_or_else(|| url.to_string());
                crate::clipboard::write(&value);
                self.toast(Toast::new("info", "Copied to clipboard"));
            }
            _ => {}
        }
    }

    fn code_description(&self, attempt: &Value, error: Option<&str>) -> Vec<(String, crate::theme::Rgba)> {
        let t = self.theme().dialog();
        let mut out = vec![(s(attempt, "instructions").to_string(), t.text.muted), (String::new(), t.text.muted), (s(attempt, "url").to_string(), t.markdown.link)];
        if let Some(e) = error {
            out.push((String::new(), t.text.muted));
            out.push((e.to_string(), t.text.feedback.error.base));
        }
        self.prewrap(out)
    }

    /// Description rows wrap the way OpenTUI does (the prompt keeps the line breaks).
    fn prewrap(&self, rows: Vec<(String, crate::theme::Rgba)>) -> Vec<(String, crate::theme::Rgba)> {
        let inner = dialog_inner(self.size.0, Size::Medium);
        rows.into_iter().map(|(text, c)| (if text.is_empty() { text } else { wrap_opentui(&text, inner).join("\n") }, c)).collect()
    }

    fn oauth_code(&mut self, ctx: Ctx, integration: Value, title: String, attempt: Value) {
        let mut p = PromptDialog::new(&title, "Authorization code", "");
        p.description = self.code_description(&attempt, None);
        self.integration_show(ctx, Step::OAuthCode { integration, attempt, settled: false }, Dialog::new(PURPOSE, Body::Prompt(p)));
    }

    fn code_submit(&mut self, token: u64, code: &str) {
        let Some(Live { ctx, step: Step::OAuthCode { integration, attempt, .. }, .. }) = self.live(token).cloned() else { return };
        if code.is_empty() {
            return;
        }
        let c = self.client.clone();
        let (id, aid, dir, code) = (s(&integration, "id").to_string(), s(&attempt, "attemptID").to_string(), ctx.dir.clone(), code.to_string());
        self.spawn(async move { c.integration_oauth_complete(&id, &aid, &code, Some(&dir)).await }, move |app, r| match r {
            Ok(()) => {
                app.settle(token);
                app.integration_connected(ctx, integration, None);
            }
            Err(e) => app.prompt_error(token, &crate::api::error_message(&e)),
        });
    }

    // ---- login commands ------------------------------------------------------------------------

    /// `CommandView`: a large dialog with the command's output in a raised band and a status line.
    fn command_view(&self, title: &str, output: &str, message: &str) -> TextDialog {
        let base = self.theme().clone();
        let t = base.dialog().clone();
        let inner = dialog_inner(self.size.0, Size::Large);
        let mut td = TextDialog::new(title, wrapped(output.trim(), inner, fg(base.text.base)));
        td.close_label = "esc close".into();
        td.band = Some(base.background.raised.high);
        td.post = wrapped(message, inner, fg(t.text.muted));
        td
    }

    fn command_start(&mut self, ctx: Ctx, integration: Value, method: Value) {
        let title = s(&method, "label").to_string();
        let mut d = Dialog::text(PURPOSE, self.command_view(&title, "", "Starting command…"));
        d.size = Size::Large;
        let token = self.integration_show(ctx.clone(), Step::CommandStarting, d);
        let c = self.client.clone();
        let (id, mid, dir) = (s(&integration, "id").to_string(), s(&method, "id").to_string(), ctx.dir.clone());
        self.spawn(async move { c.integration_command_connect(&id, &mid, Some(&dir)).await }, move |app, r| {
            let open = app.integration_shown(token);
            match r {
                Ok(attempt) => {
                    if !open {
                        let c = app.client.clone();
                        let (id, aid, dir) = (s(&integration, "id").to_string(), s(&attempt, "attemptID").to_string(), ctx.dir.clone());
                        app.spawn(async move { c.integration_command_cancel(&id, &aid, Some(&dir)).await }, |_, _| {});
                        return;
                    }
                    let mut d = Dialog::text(PURPOSE, app.command_view(&title, "", "Waiting for command to finish…"));
                    d.size = Size::Large;
                    let next = app.integration_show(ctx, Step::CommandPending { integration, attempt, settled: false }, d);
                    if let Some(d) = app.top_dialog(next) {
                        d.data["title"] = json!(title);
                    }
                    app.command_poll(next, false);
                }
                Err(e) => {
                    if open {
                        app.toast(Toast::new("error", crate::api::error_message(&e)));
                        app.dialogs.clear();
                    }
                }
            }
        });
    }

    fn command_poll(&mut self, token: u64, wait: bool) {
        let Some(Live { ctx, step: Step::CommandPending { integration, attempt, .. }, .. }) = self.live(token).cloned() else { return };
        let c = self.client.clone();
        let (id, aid, dir) = (s(&integration, "id").to_string(), s(&attempt, "attemptID").to_string(), ctx.dir.clone());
        self.spawn(
            async move {
                if wait {
                    tokio::time::sleep(std::time::Duration::from_millis(500)).await;
                }
                c.integration_command_status(&id, &aid, Some(&dir)).await
            },
            move |app, r| {
                if !app.integration_shown(token) {
                    return;
                }
                let status = match r {
                    Ok(v) => v,
                    Err(e) => {
                        app.settle(token);
                        app.toast(Toast::new("error", crate::api::error_message(&e)));
                        app.dialogs.clear();
                        return;
                    }
                };
                match s(&status, "status") {
                    "pending" => {
                        let title = app.top_dialog(token).map(|d| s(&d.data, "title").to_string()).unwrap_or_default();
                        let view = app.command_view(&title, s(&status, "message"), "Waiting for command to finish…");
                        if let Some(td) = app.top_dialog(token).and_then(|d| d.text_body()) {
                            td.lines = view.lines;
                        }
                        app.dirty = true;
                        app.command_poll(token, true);
                    }
                    "complete" => {
                        app.settle(token);
                        app.integration_connected(ctx, integration, None);
                    }
                    other => {
                        app.settle(token);
                        let message = if other == "failed" { s(&status, "message").to_string() } else { "Authentication expired".to_string() };
                        app.toast(Toast::new("error", message));
                        app.dialogs.clear();
                    }
                }
            },
        );
    }

    // ---- connected ---------------------------------------------------------------------------

    /// `connected`: reload the location's integrations, models and providers, announce it, then
    /// continue to the provider's models (or close).
    fn integration_connected(&mut self, ctx: Ctx, integration: Value, key_prompt: Option<u64>) {
        let c = self.client.clone();
        let dir = ctx.dir.clone();
        self.spawn(
            async move {
                let (i, m, p) = tokio::join!(c.integrations(Some(&dir)), c.models(Some(&dir)), c.providers(Some(&dir)));
                Ok((i?, m?, p?))
            },
            move |app, r| {
                let (i, m, p) = match r {
                    Ok(v) => v,
                    Err(e) => {
                        if let Some(token) = key_prompt {
                            app.prompt_error(token, &crate::api::error_message(&e));
                        }
                        return;
                    }
                };
                let loc = app.store.location_mut(&ctx.dir);
                loc.integrations = Some(i);
                loc.models = Some(m);
                loc.providers = Some(p);
                app.toast(Toast::new("success", format!("Connected {}", s(&integration, "name"))));
                if ctx.then_models && !is_mcp(&integration) {
                    let provider = app.integration_provider(&ctx.dir, s(&integration, "id"));
                    app.open_model_dialog(provider);
                } else {
                    app.dialogs.clear();
                }
                app.integration_catalog_changed(&ctx.dir);
                app.dirty = true;
            },
        );
    }

    /// `providerID`: the integration's provider with a usable model, else its first provider.
    fn integration_provider(&self, dir: &str, integration: &str) -> Option<String> {
        let loc = self.store.locations.get(dir)?;
        let models = loc.models.clone().unwrap_or_default();
        let matches: Vec<_> = loc.providers.clone().unwrap_or_default().into_iter().filter(|p| p.integration_id.as_deref() == Some(integration) || p.id == integration).collect();
        matches.iter().find(|p| models.iter().any(|m| m.provider_id == p.id && m.status != "deprecated")).or(matches.first()).map(|p| p.id.clone())
    }

    // ---- dialog outcomes ---------------------------------------------------------------------

    /// Outcomes of an `integration` dialog. Returns false for the generic handling (closing).
    pub fn integration_outcome(&mut self, o: &Outcome) -> bool {
        let Some(token) = self.integration_top() else { return false };
        let Some(live) = self.live(token).cloned() else { return false };
        match (o, &live.step) {
            (Outcome::Select(v), Step::List { .. }) => self.integration_list_selected(live.ctx, v),
            (Outcome::Select(v), Step::Manage { integration, methods, .. }) => self.manage_chosen(live.ctx, integration.clone(), methods.clone(), v),
            (Outcome::Moved(v), Step::Manage { .. }) => {
                if let Some(Live { step: Step::Manage { deleting, selected, .. }, .. }) = self.live(token) {
                    *selected = Some(v.clone());
                    *deleting = None;
                }
                self.manage_rebuild(token);
            }
            (Outcome::Action(a, v), Step::Manage { .. }) => self.manage_action(token, a, v.clone()),
            (Outcome::Select(v), Step::Methods) => {
                let data = self.top_dialog(token).map(|d| d.data.clone()).unwrap_or_default();
                let integration = data.get("integration").cloned().unwrap_or_default();
                let method = arr(&data, "methods").into_iter().find(|m| if v == "key" { s(m, "type") == "key" } else { s(m, "type") != "key" && s(m, "id") == v });
                if let Some(m) = method {
                    self.open_method(live.ctx, integration, m);
                }
            }
            (Outcome::Submit(text), Step::Rename { .. }) => self.rename_submit(token, text),
            (Outcome::Select(v), Step::Form(_)) => self.iform_select(token, v),
            (Outcome::Submit(text), Step::Form(_)) => self.iform_submit(token, text),
            (Outcome::Submit(text), Step::Key { .. }) => self.key_submit(token, text),
            (Outcome::Submit(text), Step::OAuthCode { .. }) => self.code_submit(token, text),
            (Outcome::Key(k), Step::OAuthAuto { .. }) => self.oauth_key(token, k),
            (Outcome::Close | Outcome::Back, Step::Form(_)) if matches!(self.dialogs.last().map(|d| &d.body), Some(Body::Choice(c)) if c.editing) => {
                // The header's `esc` while typing goes back to the answers.
                if let Some(Body::Choice(c)) = self.dialogs.last_mut().map(|d| &mut d.body) {
                    c.editing = false;
                    c.error = None;
                }
            }
            _ => return false,
        }
        self.dirty = true;
        true
    }

    // ---- the sidebar's getting-started card ----------------------------------------------------

    /// `SidebarOnboarding`: shown while nothing is connected (and not dismissed) on tall terminals.
    pub fn onboarding_visible(&self) -> bool {
        if self.size.1 < 22 {
            return false;
        }
        let Some(list) = self.loc().and_then(|l| l.integrations.as_ref()) else { return false };
        let dismissed = self.keyed.get(ONBOARDING).and_then(|v| v.get("dismissed").and_then(|d| d.as_bool())) == Some(true);
        !dismissed && !has_connected_provider(list)
    }

    /// The card's rows for a sidebar content width of `w`: a raised band (padding 1/2) with the
    /// `⬖` mark, the bold title and its `✕`, two muted notes and "Connect provider  /connect".
    pub fn onboarding_lines(&self, w: usize) -> Vec<SLine> {
        use crate::ui::text::bold;
        let t = self.theme();
        let bg = t.background.raised.high.tui();
        let inner = w.saturating_sub(4);
        let text_w = inner.saturating_sub(2).max(1);
        let pad = |n: usize| Seg::new(" ".repeat(n), ratatui::style::Style::default());
        let row = |segs: Vec<Seg>, first: bool| {
            let mut v = vec![pad(2), if first { seg("⬖", fg(t.text.base)) } else { pad(1) }, pad(1)];
            v.extend(segs);
            SLine::new(v).with_bg(Some(bg))
        };
        let mut out = vec![SLine::blank().with_bg(Some(bg))];
        let title = "Getting started";
        let gap = text_w.saturating_sub(title.len() + 1);
        out.push(row(vec![Seg::new(title, bold(fg(t.text.base))), pad(gap), seg("✕", fg(t.text.muted))], true).on_click(Some(crate::ui::text::Click::Custom("onboarding:dismiss".into()))));
        out.push(row(vec![], false));
        for note in ["OpenCode includes free models so you can start immediately.", "Connect from 75+ providers to use other models, including Claude, GPT, Gemini etc"] {
            for l in wrap(&[seg(note, fg(t.text.muted))], text_w, Wrap::Word) {
                out.push(row(l, false));
            }
            out.push(row(vec![], false));
        }
        let label = "Connect provider";
        let gap = text_w.saturating_sub(label.len() + "/connect".len());
        out.push(row(vec![seg(label, fg(t.text.base)), pad(gap), seg("/connect", fg(t.text.muted))], false).on_click(Some(crate::ui::text::Click::Custom("onboarding:connect".into()))));
        out.push(SLine::blank().with_bg(Some(bg)));
        out
    }

    pub fn onboarding_click(&mut self, what: &str) {
        match what {
            "dismiss" => {
                self.keyed.update(ONBOARDING, |v| {
                    if !v.is_object() {
                        *v = json!({});
                    }
                    v["dismissed"] = json!(true);
                });
            }
            _ => {
                self.run_command("provider.connect", None);
            }
        }
        self.dirty = true;
    }
}

/// Characters OpenTUI's word wrap may break after (besides spaces).
const BREAK_AFTER: &str = ".,;:!?-/\\()[]{}";

/// OpenTUI's word wrap at `w` cells: lines break at spaces (which hang) and after [`BREAK_AFTER`]; a
/// word that does not fit moves to the next line and is broken by characters when it is still too long.
pub fn wrap_opentui(text: &str, w: usize) -> Vec<String> {
    let w = w.max(1);
    let mut out = Vec::new();
    for paragraph in text.split('\n') {
        // Words end after a break character or before spaces; runs of spaces are their own tokens.
        let mut tokens: Vec<(bool, String)> = Vec::new();
        for g in locale::graphemes(paragraph) {
            let space = g == " ";
            let open = match tokens.last() {
                Some((s, t)) => *s == space && (space || !t.ends_with(|c: char| BREAK_AFTER.contains(c))),
                None => false,
            };
            if open {
                tokens.last_mut().unwrap().1.push_str(g);
            } else {
                tokens.push((space, g.to_string()));
            }
        }
        let mut line = String::new();
        for (space, tok) in tokens {
            let tw = locale::width(&tok);
            let used = locale::width(&line);
            if space {
                if used + tw <= w {
                    line.push_str(&tok);
                } else {
                    out.push(std::mem::take(&mut line).trim_end().to_string());
                }
                continue;
            }
            if used + tw <= w {
                line.push_str(&tok);
                continue;
            }
            if !line.trim_end().is_empty() {
                out.push(line.trim_end().to_string());
            }
            line.clear();
            for g in locale::graphemes(&tok) {
                if locale::width(&line) + locale::width(g) > w && !line.is_empty() {
                    out.push(std::mem::take(&mut line));
                }
                line.push_str(g);
            }
        }
        out.push(line.trim_end().to_string());
    }
    out
}

fn wrapped(text: &str, w: usize, style: ratatui::style::Style) -> Vec<SLine> {
    wrap_opentui(text, w).into_iter().map(|l| SLine::new(vec![Seg::new(l, style)])).collect()
}

/// `plugin.<id>.<key>` storage of the `opencode.sidebar.footer` built-in.
const ONBOARDING: &str = "plugin.opencode.sidebar.footer.getting-started";

/// The content width of a dialog of `size` on a terminal `cols` wide.
fn dialog_inner(cols: u16, size: Size) -> usize {
    (size.width().min(cols.saturating_sub(2)).max(10) as usize).saturating_sub(DIALOG_INSET * 2).max(1)
}

/// `openUrl` (`@opencode/util/open`): only http(s) links, through the platform opener.
pub fn open_url(input: &str) -> Result<(), String> {
    let url = reqwest::Url::parse(input).ok().filter(|u| u.scheme() == "http" || u.scheme() == "https");
    let Some(url) = url else { return Err(format!("Only http and https links can be opened in the browser: {input}")) };
    if cfg!(test) {
        return Ok(());
    }
    let cmd = if cfg!(target_os = "macos") { "open" } else if cfg!(windows) { "explorer" } else { "xdg-open" };
    std::process::Command::new(cmd)
        .arg(url.as_str())
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .map(|_| ())
        .map_err(|e| e.to_string())
}
