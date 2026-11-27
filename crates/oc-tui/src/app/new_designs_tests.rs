//! 2.4.1's new designs (#64988), on unless `experimental["new-designs"]` is `false`: the themes, the dialog kit and
//! select lists, the dialogs and autocomplete, each with the flag on and off.

use super::tests::{app, app_new_designs, app_with};
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
