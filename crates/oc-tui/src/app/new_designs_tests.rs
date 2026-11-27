//! 2.4.1's new designs (#64988), on unless `experimental["new-designs"]` is `false`: the themes, the dialog kit and
//! select lists, the dialogs and autocomplete, each with the flag on and off.

use super::tests::{app, app_new_designs, app_with};
use super::*;
use serde_json::json;

#[test]
fn the_flag_is_on_unless_set_to_false() {
    let cfg = |v: serde_json::Value| crate::config::Config::from_value(v).new_designs();
    assert!(cfg(json!({})));
    assert!(cfg(json!({ "experimental": {} })));
    assert!(cfg(json!({ "experimental": { "new-designs": true } })));
    assert!(cfg(json!({ "experimental": { "new-designs": "no" } })));
    assert!(!cfg(json!({ "experimental": { "new-designs": false } })));
    assert!(app_new_designs().new_designs());
    assert!(!app().new_designs());
}

/// `context/local.tsx`: agents take the categorical colors at step 400 under the new designs (200 is a muted tint
/// there), at 200 without them.
#[test]
fn agent_colors_follow_the_flag() {
    for (a, step) in [(app_new_designs(), 400), (app(), 200)] {
        let t = a.theme().clone();
        assert_eq!(a.agent_color("build"), t.categorical(0, step), "step {step}");
        assert_eq!(a.agent_color("plan"), t.categorical(1, step), "step {step}");
        assert_eq!(a.agent_color("nobody"), t.categorical(0, step), "step {step}");
    }
}

/// `prompt/index.tsx`: shell mode draws the prompt in the second categorical color at 400 under the new designs (the
/// interactive color matches the first agent's), in `text.action.primary.selected` without them.
#[test]
fn shell_mode_prompt_color_follows_the_flag() {
    for mut a in [app_new_designs(), app()] {
        a.editor.shell = true;
        let t = a.theme().clone();
        let want = if a.new_designs() { t.categorical(1, 400) } else { t.text.action.primary.selected };
        assert_eq!(a.highlight(&t), want);
    }
}

/// Every V1 theme goes through `migrateV1NewDesigns` while the flag is on; `opencode` and `opencode-classic` are V2
/// and the same either way.
#[test]
fn v1_themes_follow_the_flag() {
    let with = |on: bool| {
        let mut a = app_with(json!({ "theme": { "name": "tokyonight" }, "experimental": { "new-designs": on } }));
        a.reload_theme();
        a.theme().clone()
    };
    let (new, old) = (with(true), with(false));
    // The new layout keeps text at 100 and the page at 900.
    assert_eq!(new.text.base, new.hue("neutral", 100));
    assert_eq!(new.background.base, new.hue("neutral", 900));
    assert_eq!(old.text.base, old.hue("neutral", 200));
    assert_eq!(old.background.base, old.hue("neutral", 800));
    assert_eq!(new.text.base.hex(), old.text.base.hex());
    assert_ne!(new.background.raised.base.hex(), old.background.raised.base.hex());
    for name in ["opencode", "opencode-classic"] {
        let mut on = app_with(json!({ "theme": { "name": name } }));
        let mut off = app_with(json!({ "theme": { "name": name }, "experimental": { "new-designs": false } }));
        on.reload_theme();
        off.reload_theme();
        assert_eq!(on.theme().background.raised.base, off.theme().background.raised.base, "{name}");
    }
}

/// A `cli.json` reload that turns the experiment off migrates the V1 theme again.
#[test]
fn config_reload_follows_the_flag() {
    let mut a = app_with(json!({ "theme": { "name": "tokyonight" } }));
    let new = a.theme().background.raised.base;
    a.apply_config(crate::config::Config::from_value(json!({ "theme": { "name": "tokyonight" }, "experimental": { "new-designs": false } })));
    assert!(!a.new_designs());
    assert_ne!(a.theme().background.raised.base, new);
    a.apply_config(crate::config::Config::from_value(json!({ "theme": { "name": "tokyonight" } })));
    assert_eq!(a.theme().background.raised.base, new);
}

/// `theme.set` stores the resolved name: setting `opencode2` selects and saves `opencode`.
#[test]
fn opencode2_is_an_alias() {
    let mut a = app_new_designs();
    assert!(a.set_theme("opencode2", true));
    assert_eq!(a.themes.active, "opencode");
    assert_eq!(a.config.theme_name(), Some("opencode"));
    let a = app_with(json!({ "theme": { "name": "opencode2" } }));
    assert_eq!(a.themes.active, "opencode");
}

/// The top dialog alone on an empty screen, as upstream's component harness renders it.
fn dialog_frame(a: &mut App, w: u16, h: u16) -> ratatui::buffer::Buffer {
    let t = a.theme().clone();
    let area = ratatui::layout::Rect::new(0, 0, w, h);
    let mut buf = ratatui::buffer::Buffer::empty(area);
    crate::ui::text::fill(&mut buf, area, t.background.base.tui());
    let kb = a.keybinds.clone();
    let nd = a.new_designs();
    let d = a.dialogs.last_mut().expect("a dialog");
    crate::ui::dialog::render(&mut buf, area, d, &t, &kb, nd);
    buf
}

fn check(a: &mut App, name: &str) {
    let path = format!("{}/src/ui/testdata/nd/{name}.json", env!("CARGO_MANIFEST_DIR"));
    let fx = crate::ui::fixture::Fixture::parse(&std::fs::read_to_string(path).unwrap());
    let buf = dialog_frame(a, fx.width, fx.height);
    crate::ui::fixture::assert_matches(name, &fx, &buf);
}

fn key(a: &mut App, k: &str) {
    a.on_key(crate::keymap::Stroke::key(k));
}

fn settings_app() -> App {
    app_with(json!({ "tabs": { "mode": "off" }, "animations": false, "theme": { "mode": "dark" } }))
}

/// `dialog-config.tsx` under the new designs (E.18a): the search in the title row, each value as a
/// `DialogSelectChoice` (`opencode ↵` opens the theme list), the selected setting's description below the list.
#[test]
fn settings_dialog() {
    let mut a = settings_app();
    a.open_settings(None);
    check(&mut a, "E.18a-settings");
    let mut a = settings_app();
    a.open_settings(None);
    dialog_frame(&mut a, 80, 30);
    key(&mut a, "down");
    key(&mut a, "down");
    check(&mut a, "E.18a-settings-down");
    let mut a = settings_app();
    a.open_settings(None);
    for c in ["s", "c", "r", "o", "l", "l"] {
        key(&mut a, c);
    }
    check(&mut a, "E.18a-settings-filter");
}

/// E.18a "settings-config": values read from the config, searched by keyword.
#[test]
fn settings_dialog_values() {
    let mut a = app_with(json!({
        "tabs": { "mode": "off" }, "theme": { "mode": "dark" },
        "animations": true, "session": { "sidebar": "hide" }, "scroll": { "speed": 2.5 }, "leader": { "timeout": 2250 },
    }));
    a.open_settings(None);
    for c in ["s", "p", "e", "e", "d"] {
        key(&mut a, c);
    }
    check(&mut a, "E.18a-settings-config");
}

/// Changing a value keeps the search and the selected setting.
#[test]
fn settings_change_keeps_the_search_and_selection() {
    let mut a = settings_app();
    a.open_settings(None);
    for c in ["s", "c", "r", "o", "l", "l"] {
        key(&mut a, c);
    }
    key(&mut a, "right");
    let sel = a.dialogs.last_mut().unwrap().sel().unwrap();
    assert_eq!(sel.filter, "scroll");
    assert_eq!(sel.selected_value().as_deref(), Some("session.scrollbar"));
    assert_eq!(a.config.bool("session.scrollbar"), Some(true));
}

/// Enter on Theme opens the theme list, which returns to Settings on escape (restoring the theme) or on a pick.
#[test]
fn settings_theme_opens_the_list_and_returns() {
    let mut a = settings_app();
    a.open_settings(None);
    key(&mut a, "return");
    assert_eq!(a.dialogs.last().map(|d| d.purpose.as_str()), Some("theme"));
    key(&mut a, "down");
    assert_ne!(a.themes.active, "opencode");
    key(&mut a, "escape");
    assert_eq!(a.dialogs.last().map(|d| d.purpose.as_str()), Some("settings"));
    assert_eq!(a.themes.active, "opencode");
    assert_eq!(a.dialogs.last_mut().and_then(|d| d.sel()).and_then(|s| s.selected_value()).as_deref(), Some("theme.name"));
    key(&mut a, "return");
    key(&mut a, "down");
    let picked = a.themes.active.clone();
    key(&mut a, "return");
    assert_eq!(a.dialogs.len(), 1);
    assert_eq!(a.dialogs[0].purpose, "settings");
    assert_eq!(a.config.theme_name(), Some(picked.as_str()));
    // Legacy: enter cycles the theme in place.
    let mut a = app_with(json!({ "experimental": { "new-designs": false } }));
    a.open_settings(None);
    key(&mut a, "return");
    assert_eq!(a.dialogs.last().map(|d| d.purpose.as_str()), Some("settings"));
}

/// The E.16 catalog: the routes upstream's component fixtures serve (without the opt-in speeds).
fn models_app(connected: bool) -> App {
    let mut a = app_new_designs();
    let model = |p: &str, id: &str, name: &str, cost: f64, variants: &[&str], context: f64, status: &str| -> ModelInfo {
        serde_json::from_value(json!({
            "id": id, "modelID": id, "providerID": p, "name": name, "status": status, "enabled": true,
            "capabilities": { "input": ["text", "image"], "output": ["text"], "tools": true },
            "cost": [{ "input": cost, "output": cost * 5.0, "cache": { "read": 0, "write": 0 } }],
            "limit": { "context": context, "output": 32000 }, "time": { "released": 0 },
            "variants": variants.iter().map(|v| json!({ "id": v })).collect::<Vec<_>>(),
        }))
        .unwrap()
    };
    let loc = a.store.location_mut("/work/proj");
    loc.models = Some(vec![
        model("anthropic", "claude-sonnet", "Claude Sonnet", 3.0, &["low", "high"], 200_000.0, "active"),
        model("anthropic", "claude-haiku", "Claude Haiku", 1.0, &[], 200_000.0, "active"),
        model("openai", "gpt-5", "GPT-5", 1.25, &[], 200_000.0, "active"),
        model("openai", "gpt-4", "GPT-4", 30.0, &[], 200_000.0, "deprecated"),
        model("openai", "gpt-5-mini", "GPT-5 mini", 0.25, &[], 200_000.0, "active"),
        model("opencode", "big-pickle", "Big Pickle", 0.0, &[], 200_000.0, "active"),
        model("opencode", "grok-code", "Grok Code", 2.0, &[], 2_000_000.0, "active"),
    ]);
    loc.providers = Some(vec![
        ProviderInfo { id: "anthropic".into(), name: "Anthropic".into(), ..Default::default() },
        ProviderInfo { id: "openai".into(), name: "OpenAI".into(), ..Default::default() },
        ProviderInfo { id: "opencode".into(), name: "OpenCode".into(), ..Default::default() },
    ]);
    let connections = if connected { json!([{ "type": "credential", "id": "cred_1", "label": "Acme", "method": "oauth" }]) } else { json!([]) };
    loc.integrations = Some(vec![json!({
        "id": "opencode", "name": "OpenCode Console",
        "methods": [{ "id": "device", "type": "oauth", "label": "Sign in with OpenCode" }],
        "connections": connections,
    })]);
    loc.agents = Some(vec![
        AgentInfo {
            id: "build".into(),
            name: "build".into(),
            mode: "primary".into(),
            description: Some("The default agent".into()),
            model: Some(ModelRef { id: "claude-sonnet".into(), provider_id: "anthropic".into(), variant: Some("high".into()) }),
            ..Default::default()
        },
        AgentInfo { id: "plan".into(), name: "plan".into(), mode: "primary".into(), description: Some("Plans without editing".into()), ..Default::default() },
        AgentInfo { id: "general".into(), name: "general".into(), mode: "subagent".into(), description: Some("A subagent".into()), ..Default::default() },
    ]);
    if !connected {
        // Only OpenCode's own models: nothing connected.
        loc.providers.as_mut().unwrap().retain(|p| p.id == "opencode");
        loc.models.as_mut().unwrap().retain(|m| m.provider_id == "opencode");
    }
    a.models.add_recent("anthropic", "claude-haiku");
    a.models.set_favorite("openai", "gpt-5", true);
    a
}

/// `DialogModel` under the new designs (E.16 "model", "model-filter"): Favorites and Recent sections, `*` on
/// favorites, OpenCode's free model in a "Free" group at the end, lowercase actions; search results name the provider
/// inline and the favorite action says "unfavorite" on a favorite.
#[test]
fn model_dialog() {
    let mut a = models_app(true);
    a.open_model_dialog(None);
    check(&mut a, "E.16-model");
    let mut a = models_app(true);
    a.open_model_dialog(None);
    dialog_frame(&mut a, 90, 60);
    for c in ["g", "p", "t"] {
        key(&mut a, c);
    }
    check(&mut a, "E.16-model-filter");
}

/// Favoriting does not reorder the list until the picker is reopened.
#[test]
fn model_favorites_keep_their_place_until_reopened() {
    let mut a = models_app(true);
    a.open_model_dialog(None);
    let sel = a.dialogs.last_mut().unwrap().sel().unwrap();
    let opus = sel.visible().iter().position(|i| sel.options[*i].value == "openai/gpt-5-mini").unwrap();
    sel.selected = opus;
    a.on_key(crate::keymap::Stroke::ctrl("f"));
    assert!(a.models.is_favorite("openai", "gpt-5-mini"));
    let sel = a.dialogs.last_mut().unwrap().sel().unwrap();
    assert!(!sel.options.iter().any(|o| o.value == "section:openai/gpt-5-mini"));
    assert_eq!(sel.selected_value().as_deref(), Some("openai/gpt-5-mini"));
    assert_eq!(sel.actions[1].title, "unfavorite");
    a.dialogs.clear();
    a.open_model_dialog(None);
    let sel = a.dialogs.last_mut().unwrap().sel().unwrap();
    assert!(sel.options.iter().any(|o| o.value == "section:openai/gpt-5-mini"));
}

/// Without a connected provider the picker is "Free models": the free models only, with their facts below the list.
#[test]
fn free_models_without_a_connection() {
    let mut a = models_app(false);
    a.open_model_dialog(None);
    let buf = dialog_frame(&mut a, 90, 40);
    let text = crate::ui::fixture::buffer_text(&buf).join("\n");
    assert!(text.contains("Free models"), "{text}");
    assert!(text.contains("Connect a provider for access to more models"), "{text}");
    assert!(text.contains("Big Pickle"), "{text}");
    assert!(!text.contains("Grok"), "{text}");
    assert!(text.contains("Text, Image · 200K"), "{text}");
    assert!(text.contains("connect provider ctrl+a"), "{text}");
    // The legacy picker lists every model.
    let mut a = models_app(false);
    a.config = crate::config::Config::from_value(json!({ "experimental": { "new-designs": false } }));
    a.open_model_dialog(None);
    let text = crate::ui::fixture::buffer_text(&dialog_frame(&mut a, 90, 40)).join("\n");
    assert!(text.contains("Select model") && text.contains("Grok Code"), "{text}");
}

/// `DialogAgent` and `DialogVariant` (E.16 "agent", "variant"): the agent search in the title row, the variant list
/// without a search.
#[test]
fn agent_and_variant_dialogs() {
    let mut a = models_app(true);
    a.run_command("agent.list", None);
    check(&mut a, "E.16-agent");
    a.dialogs.clear();
    a.default_model = Some(ModelRef { id: "claude-sonnet".into(), provider_id: "anthropic".into(), variant: Some("high".into()) });
    a.run_command("variant.list", None);
    check(&mut a, "E.16-variant");
}

fn session_app() -> App {
    let mut a = app_new_designs();
    let id = "ses_newdesignsnewdesignsnewdes".to_string();
    a.store.remember(serde_json::from_value(json!({ "id": id, "projectID": "prj_1", "title": "Current", "location": { "directory": "/work/proj" }, "time": { "created": 1, "updated": 1 } })).unwrap());
    a.route = Route::Session(id);
    a.size = (100, 30);
    a
}

/// `DialogExportOptions` under the new designs (E.10a "export", "export-json"): a list of values, → changes the
/// format, and `copy c` / `save to file enter` in the footer.
#[test]
fn export_options_list() {
    let mut a = session_app();
    a.run_command("session.export", None);
    check(&mut a, "E.10a-export");
    key(&mut a, "right");
    check(&mut a, "E.10a-export-json");
    // space changes the selected row too; ↓ moves.
    key(&mut a, "down");
    key(&mut a, "space");
    let d = a.dialogs.last().unwrap();
    assert_eq!(d.data["temp"], json!(true));
    assert_eq!(d.data["json"], json!(true));
    // Legacy: the radio and checkbox form.
    let mut a = session_app();
    a.config = crate::config::Config::from_value(json!({ "experimental": { "new-designs": false } }));
    a.run_command("session.export", None);
    assert!(matches!(a.dialogs.last().unwrap().body, crate::ui::dialog::Body::Export(_)));
}

/// `DialogExportResult` under the new designs (E.10a "export-result"): `Saved to` the underlined path, `copy path c`,
/// which turns into `copied ✓`.
#[test]
fn export_result() {
    let mut a = session_app();
    let path = "/tmp/session-0b6c1f62-3d4e-4b7a-9f1e-2a5c8d7e6f40.md";
    let mut d = crate::ui::dialog::Dialog::text("export-result", super::commands::export_result(&a, path, false));
    d.data = json!({ "path": path });
    a.open_dialog(d);
    check(&mut a, "E.10a-export-result");
    key(&mut a, "c");
    let text = crate::ui::fixture::buffer_text(&dialog_frame(&mut a, 100, 30)).join("\n");
    assert!(text.contains("copied ✓"), "{text}");
}

/// `DialogDebug` under the new designs: the note is the header's description and `c` copies (enter in the legacy
/// layout, where the note sits in the footer).
#[test]
fn debug_dialog() {
    let mut a = session_app();
    a.open_debug();
    let text = crate::ui::fixture::buffer_text(&dialog_frame(&mut a, 100, 30)).join("\n");
    assert!(text.contains("Share this when reporting an issue\n"), "{text}");
    assert!(text.contains("copy c"), "{text}");
    key(&mut a, "return");
    assert!(!crate::ui::fixture::buffer_text(&dialog_frame(&mut a, 100, 30)).join("\n").contains("✓ copied"));
    key(&mut a, "c");
    assert!(crate::ui::fixture::buffer_text(&dialog_frame(&mut a, 100, 30)).join("\n").contains("✓ copied c"));
    let mut a = session_app();
    a.config = crate::config::Config::from_value(json!({ "experimental": { "new-designs": false } }));
    a.open_debug();
    let text = crate::ui::fixture::buffer_text(&dialog_frame(&mut a, 100, 30)).join("\n");
    assert!(text.contains("Share this when reporting an issue.") && text.contains("copy enter"), "{text}");
}

/// The worktree name prompt names its action "generate name" under the new designs ("generate one" before).
#[test]
fn worktree_name_action() {
    for (nd, title) in [(true, "generate name"), (false, "generate one")] {
        let mut a = app_with(json!({ "experimental": { "new-designs": nd } }));
        a.worktree_action("dialog.move_session.new", None);
        match &a.dialogs.last().unwrap().body {
            crate::ui::dialog::Body::Prompt(p) => assert_eq!(p.actions[0].0, title),
            other => panic!("{other:?}"),
        }
    }
}

/// Autocomplete under the new designs (`autocomplete.tsx`): each row draws its own left rail, the selected row's in
/// `text.formfield.focused` (the border color in the legacy layout).
#[test]
fn autocomplete_rail() {
    for nd in [true, false] {
        let mut a = if nd { app_new_designs() } else { app() };
        a.on_key(crate::keymap::Stroke::key("/"));
        let ac = a.autocomplete.clone().expect("autocomplete open");
        let buf = super::tests::render_buf(&mut a, 100, 30);
        let t = a.theme().clone();
        let selected = ac.items[ac.selected].display.trim().to_string();
        let (y, x) = (0..30u16)
            .find_map(|y| {
                let line: String = (0..100u16).map(|x| buf[(x, y)].symbol().to_string()).collect();
                line.find(&selected).map(|_| (y, line.chars().position(|c| c == '┃').unwrap() as u16))
            })
            .expect("the selected row");
        assert_eq!(buf[(x, y)].symbol(), "┃");
        let want = if nd { t.text.formfield.focused } else { t.border };
        assert_eq!(buf[(x, y)].fg, want.tui(), "nd {nd}");
        assert_eq!(buf[(x, y + 1)].fg, t.border.tui(), "nd {nd}");
    }
}
