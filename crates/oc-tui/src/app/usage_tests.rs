//! 2.2 additions to the session view: OpenCode Console usage limits (`/usage`, the model picker action,
//! the prompt label, the sidebar block, the warnings, the refusal notices; `tui/test/usage-limits.test.tsx`),
//! the `/goal` status and dialog, and the store's refresh rules.

use super::tests::{app, render};
use super::*;
use crate::keymap::Stroke;
use crate::store::{Catalog, Effect};
use serde_json::json;

fn contains(lines: &[String], needle: &str) -> bool {
    lines.iter().any(|l| l.contains(needle))
}

fn model(id: &str, name: &str, input: f64) -> ModelInfo {
    ModelInfo {
        id: id.into(),
        model_id: id.into(),
        provider_id: "opencode".into(),
        name: name.into(),
        status: "active".into(),
        enabled: true,
        cost: vec![ModelCost { input, output: input * 5.0 }],
        limit: ModelLimit { context: 1_000_000.0, output: 64_000.0 },
        ..Default::default()
    }
}

/// One request that fills Sol's context costs $4, so the $6 left won't cover three.
fn limits() -> crate::util::usage_limits::Info {
    serde_json::from_value(json!({
        "workspace": { "id": "wrk_1", "name": "Acme", "usageURL": "https://opencode.ai/wrk_1/activity/overview" },
        "budgets": [{
            "id": "picked", "limit": 50, "spent": 44, "scope": "each", "per": "set", "coverage": "models",
            "models": [
                { "providerID": "opencode", "modelID": "gpt-5.6-sol", "name": "GPT 5.6 Sol" },
                { "providerID": "opencode", "modelID": "claude-opus-5", "name": "Claude Opus 5" }
            ],
            "vendors": [], "picked": true, "shared": false, "resetsAt": 1_793_491_200_000.0
        }]
    }))
    .unwrap()
}

/// The upstream fixture: a session on GPT 5.6 Sol whose last turn the model's budget refused.
fn console_app(loaded: bool) -> (App, String) {
    let mut a = app();
    let loc = a.store.location_mut("/work/proj");
    loc.models = Some(vec![model("gpt-5.6-sol", "GPT 5.6 Sol", 4.0), model("claude-opus-5", "Claude Opus 5", 5.0)]);
    loc.providers = Some(vec![ProviderInfo { id: "opencode".into(), name: "OpenCode".into(), integration_id: Some("opencode".into()), ..Default::default() }]);
    if loaded {
        loc.usage_limits = Some(Some(limits()));
    }
    a.default_model = Some(ModelRef { id: "gpt-5.6-sol".into(), provider_id: "opencode".into(), variant: None });
    let id = "ses_usage_limits".to_string();
    a.store.remember(SessionInfo {
        id: id.clone(),
        title: Some("Usage limits".into()),
        agent: Some("build".into()),
        model: Some(ModelRef { id: "gpt-5.6-sol".into(), provider_id: "opencode".into(), variant: None }),
        location: LocationRef { directory: "/work/proj".into(), workspace_id: None },
        ..Default::default()
    });
    let body = json!({ "type": "error", "error": { "type": "BudgetLimitError", "message": "A budget policy for this model has been reached." }, "metadata": { "coverage": "models", "resetsAt": "2026-11-01T00:00:00.000Z" } }).to_string();
    let msgs: Vec<Message> = serde_json::from_value(json!([
        { "id": "msg_user", "type": "user", "text": "Refactor the parser", "time": { "created": 1 } },
        { "id": "msg_refused", "type": "assistant", "agent": "build", "model": { "providerID": "opencode", "id": "gpt-5.6-sol" }, "content": [], "finish": "error",
          "error": { "type": "provider.model-budget-limit", "message": "You've reached the spend limit for this model. Switch models to keep going.", "status": 429, "response": { "body": body } },
          "time": { "created": 2, "completed": 3 } }
    ]))
    .unwrap();
    a.store.messages.insert(id.clone(), msgs);
    a.store.loaded.insert(format!("session.message:{id}"));
    a.store.loaded.insert(format!("session.pending:{id}"));
    a.route = Route::Session(id.clone());
    (a, id)
}

#[test]
fn shows_what_the_workspaces_limits_leave_and_why_a_refused_turn_stopped() {
    let (mut a, _) = console_app(true);
    let lines = render(&mut a, 110, 36);
    let joined = lines.join("\n");
    // The refused turn says which limit stopped it and how to keep going, instead of `Error:`.
    assert!(contains(&lines, "  ┃ GPT 5.6 Sol usage limit reached"), "{joined}");
    let reset = crate::util::usage_limits::format_reset(1_793_491_200_000.0);
    let key = a.models_key();
    assert!(contains(&lines, &format!("  ┃ Resets {reset}. Switch model with {key} to continue.")), "{joined}");
    assert!(!joined.contains("Error:"), "{joined}");
    // The composer shows what's left once the model runs low.
    assert!(contains(&lines, "GPT 5.6 Sol OpenCode  $6 left"), "{joined}");
    // ...and the session warns once.
    a.tick();
    let t = a.toast.clone().expect("warning toast");
    assert_eq!((t.variant.as_str(), t.title.as_deref()), ("warning", Some("Running low on GPT 5.6 Sol")));
    assert_eq!(t.message, format!("$6 left · resets {reset}"));
    assert_eq!(t.action, Some(("View usage".into(), "usage.show".into())));
    a.toast = None;
    a.tick();
    assert!(a.toast.is_none(), "warned once per process");
}

#[test]
fn sidebar_limit_block_sits_between_context_and_mcp() {
    let (mut a, _) = console_app(true);
    let lines = render(&mut a, 160, 40);
    let joined = lines.join("\n");
    let limit = lines.iter().position(|l| l.trim_end().ends_with("Limit")).unwrap_or_else(|| panic!("{joined}"));
    assert!(lines[limit + 1].trim_end().ends_with("$6 remaining"), "{joined}");
    a.disabled_builtins.insert("opencode.sidebar.limit".into());
    let lines = render(&mut a, 160, 40);
    assert!(!contains(&lines, "$6 remaining"));
}

#[test]
fn usage_dialog_lists_limited_models_and_opens_one() {
    let (mut a, _) = console_app(true);
    a.run_command("usage.show", None);
    // Opening reloads the limits (the request goes nowhere in tests); the loaded ones stay meanwhile.
    let lines = render(&mut a, 110, 36);
    let joined = lines.join("\n");
    assert!(contains(&lines, "Usage") && contains(&lines, "Search models"), "{joined}");
    // Only the selected row shows its amount: shared money would look counted twice.
    assert!(lines.iter().any(|l| l.contains("Claude Opus 5") && l.trim_end().ends_with("$6 left")), "{joined}");
    assert!(!lines.iter().any(|l| l.contains("GPT 5.6 Sol") && l.contains("$6 left") && !l.contains("OpenCode")), "{joined}");
    a.on_key(Stroke::key("return"));
    let lines = render(&mut a, 110, 36);
    let joined = lines.join("\n");
    assert!(contains(&lines, "Usage › Claude Opus 5"), "{joined}");
    assert!(contains(&lines, "$6 remaining"), "{joined}");
    assert!(contains(&lines, "• $44 used of $50"), "{joined}");
    assert!(contains(&lines, "  Only you · GPT 5.6 Sol, Claude Opus 5"), "{joined}");
}

#[test]
fn usage_dialog_loading_signed_out_and_empty() {
    let (mut a, _) = console_app(false);
    a.run_command("usage.show", None);
    assert!(a.store.catalog_active("/work/proj", Catalog::UsageLimits));
    let lines = render(&mut a, 110, 36);
    assert!(contains(&lines, "Loading…"), "{}", lines.join("\n"));
    a.store.location_mut("/work/proj").usage_limits = Some(None);
    let lines = render(&mut a, 110, 36);
    assert!(contains(&lines, "Sign in to OpenCode Console with /connect to see your") && contains(&lines, "workspace's usage limits."), "{}", lines.join("\n"));
    let mut empty = limits();
    empty.budgets.clear();
    a.store.location_mut("/work/proj").usage_limits = Some(Some(empty));
    let lines = render(&mut a, 110, 36);
    let joined = lines.join("\n");
    assert!(contains(&lines, "No limits apply to your models. Paid models stop") && contains(&lines, "only if the workspace runs out of credits."), "{joined}");
}

#[test]
fn model_picker_offers_usage_only_with_limits() {
    let (mut a, _) = console_app(true);
    a.run_command("model.list", None);
    let lines = render(&mut a, 110, 36);
    assert!(contains(&lines, "Usage ctrl+u"), "{}", lines.join("\n"));
    a.dialogs.clear();
    a.store.location_mut("/work/proj").usage_limits = Some(None);
    a.run_command("model.list", None);
    let lines = render(&mut a, 110, 36);
    assert!(!contains(&lines, "Usage ctrl+u"), "{}", lines.join("\n"));
}

#[test]
fn prompt_loads_limits_once_for_a_console_model() {
    let (mut a, _) = console_app(false);
    assert!(!a.store.catalog_active("/work/proj", Catalog::UsageLimits));
    a.tick();
    assert!(a.store.catalog_active("/work/proj", Catalog::UsageLimits));
}

#[test]
fn go_limit_and_credit_notices() {
    let (mut a, sid) = console_app(true);
    let body = json!({ "metadata": { "limitName": "5 hour", "workspace": "wrk_01ABC" } }).to_string();
    let m = a.store.messages.get_mut(&sid).unwrap().last_mut().unwrap();
    m.error = Some(SessionError { kind: "provider.go-limit".into(), message: "Go limit".into(), response: Some(ErrorResponse { body }), ..Default::default() });
    m.touch();
    let lines = render(&mut a, 110, 36);
    let joined = lines.join("\n");
    assert!(contains(&lines, "  ┃ OpenCode Go 5 hour usage limit reached"), "{joined}");
    assert!(contains(&lines, "  ┃ Switch to a free model with"), "{joined}");
    assert!(contains(&lines, "  ┃ Or turn on Extra Usage on your Go page to keep going now:"), "{joined}");
    assert!(contains(&lines, "  ┃ https://opencode.ai/console/wrk_01ABC/go"), "{joined}");
    // Out of credits on a Console-served model.
    let m = a.store.messages.get_mut(&sid).unwrap().last_mut().unwrap();
    m.error = Some(SessionError { kind: "provider.unknown".into(), message: "Payment required".into(), status: Some(402), ..Default::default() });
    m.touch();
    let lines = render(&mut a, 110, 36);
    let joined = lines.join("\n");
    assert!(contains(&lines, "  ┃ Acme is out of credits"), "{joined}");
    assert!(contains(&lines, "  ┃ Paid models are paused until an admin adds credits in OpenCode Console."), "{joined}");
}

fn ev(kind: &str, data: serde_json::Value, dir: Option<&str>) -> Event {
    Event {
        id: format!("evt_{}", crate::util::id_body(false)),
        kind: kind.into(),
        data,
        location: dir.map(|d| LocationRef { directory: d.into(), workspace_id: None }),
        ..Default::default()
    }
}

#[test]
fn store_refetches_only_loaded_catalogs() {
    let mut s = crate::store::Store::new("/work/proj");
    // Nothing loaded: an update for another location only invalidates.
    assert!(s.apply(&ev("integration.updated", json!({}), Some("/elsewhere"))).is_empty());
    assert!(s.apply(&ev("vcs.branch.updated", json!({ "branch": "dev" }), Some("/elsewhere"))).is_empty());
    assert!(s.locations.get("/elsewhere").is_none());
    // Loaded catalogs reload; the branch event refetches VCS instead of patching it.
    s.mark_requested("/work/proj", Catalog::Vcs);
    s.mark_requested("/work/proj", Catalog::Models);
    assert_eq!(s.apply(&ev("vcs.branch.updated", json!({ "branch": "dev" }), Some("/work/proj"))), vec![Effect::Catalog("/work/proj".into(), Catalog::Vcs)]);
    assert_eq!(
        s.apply(&ev("integration.updated", json!({}), Some("/work/proj"))),
        vec![Effect::Catalog("/work/proj".into(), Catalog::Integrations), Effect::Catalog("/work/proj".into(), Catalog::Models), Effect::Catalog("/work/proj".into(), Catalog::Providers)]
    );
    // Budget refusals reload loaded usage limits.
    let failed = |kind: &str| ev("session.step.failed", json!({ "sessionID": "ses_x", "assistantMessageID": "msg_x", "error": { "type": kind, "message": "x" } }), Some("/work/proj"));
    assert!(s.apply(&failed("provider.budget-limit")).is_empty());
    s.mark_requested("/work/proj", Catalog::UsageLimits);
    assert_eq!(s.apply(&failed("provider.model-budget-limit")), vec![Effect::Catalog("/work/proj".into(), Catalog::UsageLimits)]);
    assert!(s.apply(&failed("provider.rate-limit")).is_empty());
}

// ---- /goal ---------------------------------------------------------------------------------

fn goal(status: &str, reason: Option<&str>) -> serde_json::Value {
    json!({ "id": "goal_1", "objective": "Make every parser test pass", "status": status, "reason": reason, "rounds": 3, "maxRounds": 50, "boot": "b", "updatedAt": 1 })
}

#[test]
fn goal_status_follows_changed_events_and_opens_the_dialog() {
    let (mut a, sid) = console_app(true);
    a.tick();
    a.toast = None;
    assert!(a.goal().is_none());
    // No goal: the footer shows nothing and a click explains how to start one.
    a.on_click(crate::ui::text::Click::Custom(super::goal::CLICK.into()));
    assert_eq!(a.toast.as_ref().map(|t| t.message.as_str()), Some("No goal is set. Start one with /goal <objective>."));
    a.toast = None;
    // Events for other sessions are ignored.
    a.on_event(ev(super::goal::CHANGED, json!({ "sessionID": "ses_other", "goal": goal("active", None) }), Some("/work/proj")));
    assert!(a.goal().is_none());
    a.on_event(ev(super::goal::CHANGED, json!({ "sessionID": sid, "goal": goal("active", None) }), Some("/work/proj")));
    let lines = render(&mut a, 110, 36);
    let joined = lines.join("\n");
    let footer = lines.iter().find(|l| l.contains("◎ goal")).unwrap_or_else(|| panic!("{joined}"));
    // The goal alone hides the agents hint and never shows rounds.
    assert!(!footer.contains("agents") && !footer.contains("3/50"), "{footer}");
    // A click on the item opens the dialog.
    let hit = a.hits.iter().find(|(_, c)| *c == crate::ui::text::Click::Custom(super::goal::CLICK.into())).cloned().expect("goal hit");
    a.on_click(hit.1);
    let lines = render(&mut a, 110, 36);
    let joined = lines.join("\n");
    assert!(contains(&lines, "Goal") && contains(&lines, "Make every parser test pass") && contains(&lines, "◎ Active"), "{joined}");
    assert!(contains(&lines, "pause p") && contains(&lines, "clear x") && !contains(&lines, "resume r"), "{joined}");
    // A pause arrives as an event; the dialog follows it.
    a.on_event(ev(super::goal::CHANGED, json!({ "sessionID": sid, "goal": goal("paused", Some("Paused by the user")) }), Some("/work/proj")));
    let lines = render(&mut a, 110, 36);
    let joined = lines.join("\n");
    assert!(contains(&lines, "◎ Paused") && contains(&lines, "Paused by the user") && contains(&lines, "resume r"), "{joined}");
    a.dialogs.clear();
    let lines = render(&mut a, 110, 36);
    assert!(contains(&lines, "◎ goal paused"), "{}", lines.join("\n"));
    a.on_event(ev(super::goal::CHANGED, json!({ "sessionID": sid, "goal": goal("complete", None) }), Some("/work/proj")));
    let lines = render(&mut a, 110, 36);
    assert!(contains(&lines, "◎ goal met"), "{}", lines.join("\n"));
    // Clearing removes it.
    a.on_event(ev(super::goal::CHANGED, json!({ "sessionID": sid, "goal": null }), Some("/work/proj")));
    let lines = render(&mut a, 110, 36);
    assert!(!contains(&lines, "◎ goal"));
}

#[test]
fn goal_kickoff_notice_uses_the_goal_icon() {
    let (mut a, sid) = console_app(true);
    let m: Message = serde_json::from_value(json!({ "id": "msg_goal", "type": "synthetic", "text": "x", "description": "Goal: Make every parser test pass", "metadata": { "source": "goal" }, "time": { "created": 4 } })).unwrap();
    a.store.messages.get_mut(&sid).unwrap().push(m);
    let lines = render(&mut a, 110, 36);
    assert!(contains(&lines, "◎ Goal: Make every parser test pass"), "{}", lines.join("\n"));
}

#[test]
fn toast_click_runs_its_action() {
    let (mut a, _) = console_app(true);
    a.tick();
    assert!(a.toast.is_some());
    let _ = render(&mut a, 110, 36);
    let hit = a.hits.first().cloned().expect("toast hit");
    assert_eq!(hit.1, crate::ui::text::Click::Custom(super::view::TOAST_CLICK.into()));
    a.on_click(hit.1);
    assert!(a.toast.is_none());
    assert_eq!(a.dialogs.last().map(|d| d.purpose.as_str()), Some(super::usage::PURPOSE));
}
