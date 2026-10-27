//! The free usage limit (`routes/session/index.tsx`, `component/dialog-go-upsell.tsx`): when a
//! step fails with `provider.free-limit`, the prompt of that turn comes back into an empty
//! composer and, at most once a day unless declined for good, the OpenCode Go offer opens. Its
//! `subscribe` opens the Go page and signs in to the `opencode` integration with the device flow.

use super::{App, Toast};
use crate::api::types::*;
use crate::ui::dialog::{Body, Dialog, Footer, FooterItem, TextDialog};
use crate::ui::go_upsell::{GO_URL, GoUpsell};
use crate::ui::text::{SLine, fg, seg};
use serde_json::{Value, json};

const DAY: i64 = 86_400_000;
const STORAGE: &str = "go-upsell";

/// `projectedPromptInput`: a user message as an editable prompt (attachments keep their mentions).
pub fn projected_prompt(m: &Message) -> crate::state::PromptInfo {
    let mut p = crate::state::PromptInfo::empty();
    p.text = m.text_str().to_string();
    for f in m.files.iter().flatten() {
        let v = serde_json::to_value(f).unwrap_or(Value::Null);
        let uri = match v.pointer("/source/type").and_then(|t| t.as_str()) {
            Some("uri") => v.pointer("/source/uri").and_then(|u| u.as_str()).unwrap_or("").to_string(),
            _ => format!("data:{};base64,{}", v.get("mime").and_then(|m| m.as_str()).unwrap_or(""), v.get("data").and_then(|d| d.as_str()).unwrap_or("")),
        };
        let mut file = json!({ "uri": uri });
        for k in ["name", "description", "mention"] {
            if let Some(x) = v.get(k).filter(|x| !x.is_null()) {
                file[k] = x.clone();
            }
        }
        p.files.push(file);
    }
    for a in m.agents.iter().flatten() {
        let v = serde_json::to_value(a).unwrap_or(Value::Null);
        let mut agent = json!({ "name": v.get("name").cloned().unwrap_or(Value::Null) });
        if let Some(x) = v.get("mention").filter(|x| !x.is_null()) {
            agent["mention"] = x.clone();
        }
        p.agents.push(agent);
    }
    let skills: Vec<Value> = m
        .skills
        .iter()
        .flatten()
        .map(|s| {
            let v = serde_json::to_value(s).unwrap_or(Value::Null);
            let mut skill = json!({ "id": v.get("id").cloned().unwrap_or(Value::Null) });
            if let Some(x) = v.get("mention").filter(|x| !x.is_null()) {
                skill["mention"] = x.clone();
            }
            skill
        })
        .collect();
    p.skills = Some(skills);
    p
}

fn prompt_empty(p: &crate::state::PromptInfo) -> bool {
    p.text.trim().is_empty() && p.files.is_empty() && p.agents.is_empty() && p.skills.as_ref().is_none_or(|s| s.is_empty()) && p.pasted.is_empty()
}

/// The parts of a prompt the offer compares to decide whether the recalled prompt is still there.
fn prompt_shape(p: &crate::state::PromptInfo) -> Value {
    json!([p.text, p.files.len(), p.agents.len(), p.skills.as_ref().map(|s| s.len()).unwrap_or(0), p.pasted.len()])
}

impl App {
    /// `session.step.failed` with the free-limit error on the routed session.
    pub fn free_limit_hit(&mut self, ev: &Event) {
        if ev.data.pointer("/error/type").and_then(|t| t.as_str()) != Some("provider.free-limit") {
            return;
        }
        let Some(sid) = ev.session_id().map(|s| s.to_string()) else { return };
        if self.session_id() != Some(sid.as_str()) {
            return;
        }
        let Some(aid) = ev.str("assistantMessageID").map(|s| s.to_string()) else { return };
        if !self.free_limits_handled.insert(aid.clone()) {
            return;
        }
        if !prompt_empty(&self.editor.info()) {
            return;
        }
        // Put the turn's prompt back: the last user message before the failed step, unless it is
        // still pending in the inbox; a shell or idle row ends the turn.
        let list = self.store.messages(&sid).to_vec();
        if let Some(end) = list.iter().position(|m| m.id == aid) {
            for m in list[..end].iter().rev() {
                if m.kind == "shell" || m.kind == "idle" {
                    break;
                }
                if m.kind == "user" && !self.store.pending_has_input(&sid, &m.id) {
                    let p = projected_prompt(m);
                    self.editor.set(&p);
                    self.editor.buffer_end();
                    break;
                }
            }
        }
        if !self.dialogs.is_empty() {
            return;
        }
        let stored = self.keyed.get(STORAGE).unwrap_or(Value::Null);
        let stored_seen = stored.get("lastSeenAt").and_then(|v| v.as_i64());
        let dont_show = stored.get("dontShow").and_then(|v| v.as_bool()) == Some(true);
        if dont_show || self.go_upsell_memory.1 {
            if dont_show {
                self.go_upsell_memory = (stored_seen.or(self.go_upsell_memory.0), true);
            }
            return;
        }
        let now = self.now();
        let seen = stored_seen.unwrap_or(0).max(self.go_upsell_memory.0.unwrap_or(0));
        if seen > 0 && now - seen < DAY {
            return;
        }
        self.go_upsell_memory.0 = Some(now);
        self.keyed.update(STORAGE, |v| {
            if !v.is_object() {
                *v = json!({});
            }
            v["lastSeenAt"] = json!(now);
        });
        let retry = ev.data.pointer("/error/retryAfterMs").and_then(|v| v.as_f64());
        let reset = crate::util::locale::free_limit_reset(retry, ev.created, now);
        let mut d = Dialog::new("go-upsell", Body::GoUpsell(Box::new(GoUpsell::new(reset, self.config.animations()))));
        d.data = json!({ "session": sid, "prompt": prompt_shape(&self.editor.info()) });
        self.open_dialog(d);
    }

    /// `canContinue`: the recalled prompt is still in the composer of the same session.
    fn go_prompt_intact(&self, expected: &Value) -> bool {
        let sid = expected.get("session").and_then(|s| s.as_str());
        sid.is_some() && self.session_id() == sid && expected.get("prompt") == Some(&prompt_shape(&self.editor.info()))
    }

    /// An offer button (`don't show again`, `dismiss`, `subscribe`).
    pub fn go_upsell_action(&mut self, action: &str) {
        let data = self.dialogs.last().map(|d| d.data.clone()).unwrap_or(Value::Null);
        match action {
            "subscribe" => {
                super::commands::open_url(GO_URL);
                self.go_connect(data);
            }
            "don't show again" => {
                self.go_upsell_memory.1 = true;
                self.keyed.update(STORAGE, |v| {
                    if !v.is_object() {
                        *v = json!({});
                    }
                    v["dontShow"] = json!(true);
                });
                self.dialogs.clear();
            }
            _ => self.dialogs.clear(),
        }
        self.dirty = true;
    }

    fn opencode_integration(&self) -> Option<Value> {
        self.loc().and_then(|l| l.integrations.as_ref()).and_then(|list| list.iter().find(|i| i.get("id").and_then(|x| x.as_str()) == Some("opencode")).cloned())
    }

    /// An active credential connection without a status problem.
    fn opencode_connected(&self) -> bool {
        self.opencode_integration().is_some_and(|i| {
            i.get("connections")
                .and_then(|c| c.as_array())
                .and_then(|a| a.iter().find(|c| c.get("type").and_then(|t| t.as_str()) == Some("credential")))
                .is_some_and(|c| c.get("status").is_none_or(|s| s.is_null()))
        })
    }

    /// `DialogGoUpsellConnect`: the device sign-in for the `opencode` integration.
    fn go_connect(&mut self, data: Value) {
        let integration = self.opencode_integration();
        let method = integration.as_ref().and_then(|i| {
            i.get("methods")
                .and_then(|m| m.as_array())
                .and_then(|a| a.iter().find(|m| m.get("type").and_then(|t| t.as_str()) == Some("oauth") && m.get("id").and_then(|x| x.as_str()) == Some("device")).cloned())
        });
        let title = method.as_ref().and_then(|m| m.get("label").and_then(|l| l.as_str())).unwrap_or("Connect an integration").to_string();
        let t = self.theme().dialog().clone();
        let mut d = Dialog::text("go-connect", TextDialog::new(title, vec![SLine::new(vec![seg("Starting authorization…", fg(t.text.muted))])]));
        d.data = json!({ "upsell": data });
        self.open_dialog(d);
        let Some(method) = method else { return };
        if self.opencode_connected() {
            self.go_sync(false, Some(method));
            return;
        }
        self.go_start_oauth(method);
    }

    fn go_connect_open(&self) -> bool {
        self.dialogs.last().is_some_and(|d| d.purpose == "go-connect")
    }

    fn go_start_oauth(&mut self, method: Value) {
        let mut answer = serde_json::Map::new();
        for field in method.get("form").and_then(|f| f.as_array()).into_iter().flatten() {
            if field.get("type").and_then(|t| t.as_str()) != Some("external")
                && let (Some(k), Some(v)) = (field.get("key").and_then(|k| k.as_str()), field.get("default"))
            {
                answer.insert(k.to_string(), v.clone());
            }
        }
        let id = method.get("id").and_then(|i| i.as_str()).unwrap_or("device").to_string();
        let c = self.client.clone();
        let dir = self.location_dir();
        self.spawn(async move { c.integration_oauth_connect("opencode", &id, if answer.is_empty() { None } else { Some(Value::Object(answer)) }, Some(&dir)).await }, |app, r| match r {
            Ok(attempt) => {
                let attempt_id = attempt.get("attemptID").and_then(|a| a.as_str()).unwrap_or("").to_string();
                if !app.go_connect_open() {
                    let c = app.client.clone();
                    let dir = app.location_dir();
                    app.spawn(async move { c.integration_oauth_cancel("opencode", &attempt_id, Some(&dir)).await }, |_, _| {});
                    return;
                }
                app.go_show_attempt(&attempt);
                app.go_poll(attempt_id);
            }
            Err(e) => {
                if app.go_connect_open() {
                    app.toast(Toast::new("error", crate::api::error_message(&e)));
                    app.dialogs.clear();
                }
            }
        });
    }

    fn go_show_attempt(&mut self, attempt: &Value) {
        let t = self.theme().dialog().clone();
        let url = attempt.get("url").and_then(|u| u.as_str()).unwrap_or("").to_string();
        let instructions = attempt.get("instructions").and_then(|u| u.as_str()).unwrap_or("").to_string();
        let mut lines = vec![SLine::new(vec![seg(url, fg(t.markdown.link))])];
        if !instructions.is_empty() {
            lines.push(SLine::blank());
            lines.push(SLine::new(vec![seg(instructions, fg(t.text.muted))]));
        }
        lines.push(SLine::blank());
        lines.push(SLine::new(vec![seg("Waiting for authorization…", fg(t.text.muted))]));
        if let Some(d) = self.dialogs.last_mut().filter(|d| d.purpose == "go-connect") {
            d.data["attempt"] = attempt.clone();
            if let Some(td) = d.text_body() {
                td.lines = lines;
                td.footer = Some(Footer { secondary: vec![FooterItem::new("open", "o"), FooterItem::new("copy", "c")], ..Default::default() });
            }
        }
        self.dirty = true;
    }

    fn go_poll(&mut self, attempt_id: String) {
        let c = self.client.clone();
        let dir = self.location_dir();
        let id = attempt_id.clone();
        self.spawn(
            async move {
                tokio::time::sleep(std::time::Duration::from_millis(500)).await;
                c.integration_oauth_status("opencode", &id, Some(&dir)).await
            },
            move |app, r| {
                let current = app.dialogs.last().filter(|d| d.purpose == "go-connect").and_then(|d| d.data.pointer("/attempt/attemptID").and_then(|a| a.as_str()).map(|s| s.to_string()));
                if current.as_deref() != Some(attempt_id.as_str()) {
                    return;
                }
                match r {
                    Ok(status) => match status.get("status").and_then(|s| s.as_str()) {
                        Some("pending") => app.go_poll(attempt_id),
                        Some("complete") => {
                            app.go_settle();
                            app.go_sync(true, None);
                        }
                        Some("failed") => {
                            app.go_settle();
                            let msg = status.get("message").and_then(|m| m.as_str()).unwrap_or("Authorization failed").to_string();
                            app.toast(Toast::new("error", msg));
                            app.dialogs.clear();
                        }
                        _ => {
                            app.go_settle();
                            app.toast(Toast::new("error", "Authorization expired"));
                            app.dialogs.clear();
                        }
                    },
                    Err(e) => {
                        app.go_settle();
                        app.toast(Toast::new("error", crate::api::error_message(&e)));
                        app.dialogs.clear();
                    }
                }
            },
        );
    }

    /// The attempt finished: closing the dialog no longer cancels it.
    fn go_settle(&mut self) {
        if let Some(d) = self.dialogs.last_mut().filter(|d| d.purpose == "go-connect") {
            d.data["settled"] = json!(true);
        }
    }

    /// Cancel an unsettled attempt when its dialog closes.
    pub fn go_connect_closed(&mut self, d: &Dialog) {
        if d.data.get("settled") == Some(&json!(true)) {
            return;
        }
        let Some(attempt) = d.data.pointer("/attempt/attemptID").and_then(|a| a.as_str()).map(|s| s.to_string()) else { return };
        let c = self.client.clone();
        let dir = self.location_dir();
        self.spawn(async move { c.integration_oauth_cancel("opencode", &attempt, Some(&dir)).await }, |_, _| {});
    }

    /// Reload integrations, models and providers; then continue (`finish`) once connected.
    fn go_sync(&mut self, announce: bool, method: Option<Value>) {
        let c = self.client.clone();
        let dir = self.location_dir();
        self.spawn(
            async move {
                let (i, m, p) = tokio::join!(c.integrations(Some(&dir)), c.models(Some(&dir)), c.providers(Some(&dir)));
                Ok((i?, m?, p?))
            },
            move |app, r| {
                if !app.go_connect_open() {
                    return;
                }
                match r {
                    Ok((i, m, p)) => {
                        let dir = app.location_dir();
                        let loc = app.store.location_mut(&dir);
                        loc.integrations = Some(i);
                        loc.models = Some(m);
                        loc.providers = Some(p);
                        if !app.opencode_connected() {
                            match method {
                                Some(m) => app.go_start_oauth(m),
                                None => app.dialogs.clear(),
                            }
                            return;
                        }
                        if announce {
                            let name = app.opencode_integration().and_then(|i| i.get("name").and_then(|n| n.as_str()).map(|s| s.to_string())).unwrap_or_else(|| "OpenCode Console".into());
                            app.toast(Toast::new("success", format!("Connected {name}")));
                        }
                        app.go_finish();
                    }
                    Err(e) => {
                        app.toast(Toast::new("error", crate::api::error_message(&e)));
                        app.dialogs.clear();
                    }
                }
                app.dirty = true;
            },
        );
    }

    /// Switch to an OpenCode Go model and resend the recalled prompt, or pick a model.
    fn go_finish(&mut self) {
        let upsell = self.dialogs.last().and_then(|d| d.data.get("upsell").cloned()).unwrap_or(Value::Null);
        if !self.go_prompt_intact(&upsell) {
            self.dialogs.clear();
            return;
        }
        let model = self.model_list().into_iter().find(|m| m.provider_id == "opencode-go" && m.status != "deprecated");
        match model {
            Some(m) => {
                self.set_model(ModelRef { provider_id: m.provider_id.clone(), id: m.id.clone(), variant: None }, true);
                self.dialogs.clear();
                self.submit("steer");
            }
            None => {
                self.dialogs.clear();
                self.open_model_dialog(None);
            }
        }
    }

    /// `o` / `c` on the sign-in dialog.
    pub fn go_connect_key(&mut self, key: &str) {
        let Some(attempt) = self.dialogs.last().and_then(|d| d.data.get("attempt").cloned()) else { return };
        let url = attempt.get("url").and_then(|u| u.as_str()).unwrap_or("").to_string();
        match key {
            "o" => super::commands::open_url(&url),
            "c" => {
                let instructions = attempt.get("instructions").and_then(|u| u.as_str()).unwrap_or("");
                static CODE: once_cell::sync::Lazy<regex::Regex> = once_cell::sync::Lazy::new(|| regex::Regex::new(r"[A-Z0-9]{4}-[A-Z0-9]{4,5}").unwrap());
                let value = CODE.find(instructions).map(|m| m.as_str().to_string()).unwrap_or(url);
                crate::clipboard::write(&value);
                self.toast(Toast::new("info", "Copied to clipboard"));
            }
            _ => {}
        }
    }
}
