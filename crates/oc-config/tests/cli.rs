//! Port of `packages/cli/test/config.test.ts`.

use std::path::{Path, PathBuf};

use oc_config::cli::CliConfig;
use oc_config::cli::migrate::parse_jsonc;
use serde_json::{Value, json};

struct Tmp(PathBuf);
impl Tmp {
    fn new() -> Tmp {
        let dir = std::env::temp_dir().join(format!("oc-cli-config-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        Tmp(dir)
    }
    fn path(&self) -> &Path {
        &self.0
    }
    fn file(&self, name: &str) -> PathBuf {
        self.0.join(name)
    }
    fn write(&self, name: &str, text: &str) {
        std::fs::write(self.file(name), text).unwrap();
    }
    fn read(&self, name: &str) -> String {
        std::fs::read_to_string(self.file(name)).unwrap()
    }
    fn json(&self, name: &str) -> Value {
        parse_jsonc(&self.read(name)).unwrap()
    }
}
impl Drop for Tmp {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn service(tmp: &Tmp, content: Option<&str>) -> CliConfig {
    CliConfig::with_content(tmp.path(), tmp.path(), content)
}

fn rt<F: std::future::Future>(f: F) -> F::Output {
    tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap().block_on(f)
}

#[test]
fn includes_the_published_schema_when_creating_cli_json() {
    let tmp = Tmp::new();
    let config = rt(service(&tmp, None).update(|d| {
        d["animations"] = json!(false);
    }))
    .unwrap();
    assert_eq!(config, json!({ "$schema": "https://opencode.ai/v2/cli.json", "animations": false }));
    assert_eq!(tmp.json("cli.json"), config);
    assert!(tmp.read("cli.json").ends_with('\n'));
}

#[test]
fn preserves_the_schema_in_an_existing_cli_json() {
    let tmp = Tmp::new();
    tmp.write("cli.json", r#"{"$schema":"https://opencode.ai/v2/cli.json","animations":true}"#);
    let config = rt(service(&tmp, None).update(|d| d["animations"] = json!(false))).unwrap();
    assert_eq!(config, json!({ "$schema": "https://opencode.ai/v2/cli.json", "animations": false }));
    assert_eq!(tmp.json("cli.json"), config);
}

#[test]
fn merges_inline_cli_config_content_over_the_global_config() {
    let tmp = Tmp::new();
    tmp.write(
        "cli.json",
        &json!({ "tabs": { "mode": "on", "scope": "global" }, "keybinds": { "app.exit": "ctrl+q" }, "plugins": ["global"], "animations": true }).to_string(),
    );
    let content = json!({ "tabs": { "enabled": false }, "keybinds": { "help.show": false }, "plugins": ["inline"], "animations": false }).to_string();
    let svc = service(&tmp, Some(&content));
    let loaded = rt(svc.get());
    assert_eq!(loaded["tabs"], json!({ "mode": "off", "scope": "global" }));
    assert_eq!(loaded["keybinds"], json!({ "app.exit": "ctrl+q", "help.show": false }));
    assert_eq!(loaded["plugins"], json!(["inline"]));
    let updated = rt(svc.update(|d| {
        d["animations"] = json!(true);
        d["mouse"] = json!(false);
    }))
    .unwrap();
    assert_eq!(updated["animations"], json!(false));
    assert_eq!(updated["mouse"], json!(false));
    let file = tmp.json("cli.json");
    assert_eq!(file["animations"], json!(true));
    assert_eq!(file["mouse"], json!(false));
}

#[test]
fn reads_the_legacy_tabs_toggle_without_rewriting_it() {
    let tmp = Tmp::new();
    tmp.write("cli.json", r#"{"tabs":{"enabled":false}}"#);
    let svc = service(&tmp, None);
    assert_eq!(rt(svc.get())["tabs"], json!({ "mode": "off" }));
    let config = rt(svc.update(|d| d["animations"] = json!(false))).unwrap();
    assert_eq!(config["tabs"], json!({ "mode": "off" }));
    assert_eq!(tmp.json("cli.json"), json!({ "tabs": { "enabled": false }, "animations": false }));
}

#[test]
fn migrates_tui_and_kv_config_into_cli_json() {
    let tmp = Tmp::new();
    tmp.write(
        "tui.json",
        &json!({
            "theme": "legacy",
            "keybinds": {
                "leader": "ctrl+o", "app_exit": "ctrl+q", "app_heap_snapshot": "ctrl+h",
                "input_paste": { "key": "ctrl+v", "preventDefault": false },
                "session_delete": false, "dialog.select.next": "ctrl+n",
            },
            "leader_timeout": 500, "scroll_speed": 2, "scroll_acceleration": { "enabled": true },
            "diff_style": "stacked", "mouse": false,
        })
        .to_string(),
    );
    tmp.write(
        "kv.json",
        &json!({
            "theme_mode_lock": "light", "attention_sound_pack": "custom.pack", "diff_wrap_mode": "none",
            "diff_viewer_show_file_tree": false, "diff_viewer_single_patch": true, "diff_viewer_view": "split",
            "terminal_title_enabled": false, "file_context_enabled": false, "paste_summary_enabled": false,
            "sidebar": "hide", "scrollbar_visible": true, "thinking_mode": "show", "exploration_grouping": false,
            "dismissed_getting_started": true, "animations_enabled": false, "skipped_version": "9.9.9",
            "which_key_layout": "overlay",
        })
        .to_string(),
    );
    let config = rt(service(&tmp, None).get());
    let keybinds = json!({
        "leader": "ctrl+o", "app.exit": "ctrl+q",
        "prompt.paste": { "key": "ctrl+v", "preventDefault": false },
        "session.delete": false, "dialog.select.next": "ctrl+n",
    });
    assert_eq!(config["$schema"], json!("https://opencode.ai/v2/cli.json"));
    assert_eq!(config["theme"], json!({ "name": "legacy", "mode": "light" }));
    assert_eq!(config["keybinds"], keybinds);
    assert_eq!(config["leader"], json!({ "timeout": 500 }));
    assert_eq!(config["scroll"], json!({ "speed": 2, "acceleration": true }));
    assert_eq!(config["attention"], json!({ "sound_pack": "custom.pack" }));
    assert_eq!(config["diffs"], json!({ "wrap": "none", "tree": false, "single": true, "view": "split" }));
    assert_eq!(config["terminal"], json!({ "title": false }));
    assert_eq!(config["prompt"], json!({ "editor": false, "paste": "full" }));
    assert_eq!(config["session"], json!({ "sidebar": "hide", "scrollbar": true, "thinking": "show", "grouping": "none" }));
    assert_eq!(config["animations"], json!(false));
    assert_eq!(config["mouse"], json!(false));
    assert!(config.get("skipped_version").is_none());
    assert_eq!(tmp.json("cli.json")["keybinds"], keybinds);
    assert!(tmp.file("tui.json").exists() && tmp.file("kv.json").exists());
}

#[test]
fn migrates_before_the_first_update_and_does_not_remigrate_afterward() {
    let tmp = Tmp::new();
    tmp.write("tui.json", r#"{"theme":"legacy"}"#);
    let svc = service(&tmp, None);
    rt(svc.update(|d| {
        d["animations"] = json!(false);
        d["mouse"] = json!(false);
    }))
    .unwrap();
    tmp.write("tui.json", r#"{"theme":"changed"}"#);
    let config = rt(svc.get());
    let expected =
        json!({ "$schema": "https://opencode.ai/v2/cli.json", "theme": { "name": "legacy" }, "animations": false, "mouse": false });
    assert_eq!(config, expected);
    assert_eq!(tmp.json("cli.json"), expected);
}

#[test]
fn preserves_legacy_cursor_settings() {
    let tmp = Tmp::new();
    tmp.write("tui.json", r#"{"cursor":{"style":"underline","blinking":false}}"#);
    let config = rt(service(&tmp, None).get());
    assert_eq!(config["cursor"], json!({ "style": "underline", "blinking": false }));
    assert_eq!(tmp.json("cli.json")["cursor"], json!({ "style": "underline", "blinking": false }));
}

#[test]
fn migrates_legacy_keybind_names_in_an_existing_cli_json() {
    let tmp = Tmp::new();
    tmp.write(
        "cli.json",
        r#"{
  // Preserve this comment
  "keybinds": {
    // Session list shortcut
    "session_list": "ctrl+l",
    "app_heap_snapshot": "ctrl+h",
    // Legacy delete shortcut
    "session_delete": "ctrl+d",
    // Canonical delete shortcut
    "session.delete": "ctrl+x",
    "app.heap_snapshot": "ctrl+shift+h"
  }
}
"#,
    );
    let config = rt(service(&tmp, None).get());
    assert_eq!(config["keybinds"], json!({ "session.list": "ctrl+l", "session.delete": "ctrl+x" }));
    let text = tmp.read("cli.json");
    for c in ["// Preserve this comment", "// Session list shortcut", "// Legacy delete shortcut", "// Canonical delete shortcut"] {
        assert!(text.contains(c), "{c} missing from {text}");
    }
    assert_eq!(parse_jsonc(&text).unwrap()["keybinds"], json!({ "session.list": "ctrl+l", "session.delete": "ctrl+x" }));
}

#[test]
fn migrates_copy_on_select_in_an_existing_cli_json() {
    for (legacy, copy, expected) in [(true, None, "select"), (false, None, "manual"), (true, Some("manual"), "manual")] {
        let tmp = Tmp::new();
        let copy_line = copy.map(|c| format!("\"copy\": \"{c}\",")).unwrap_or_default();
        tmp.write(
            "cli.json",
            &format!("{{\n  // Preserve this comment\n  \"terminal\": {{\n    {copy_line}\n    \"copy_on_select\": {legacy}\n  }}\n}}\n"),
        );
        let config = rt(service(&tmp, None).get());
        assert_eq!(config["terminal"], json!({ "copy": expected }));
        let text = tmp.read("cli.json");
        assert!(text.contains("// Preserve this comment"));
        assert!(!text.contains("copy_on_select"));
        assert_eq!(parse_jsonc(&text).unwrap()["terminal"], json!({ "copy": expected }));
    }
}

#[test]
fn preserves_the_effective_value_when_migrating_duplicate_legacy_keybinds() {
    let tmp = Tmp::new();
    tmp.write("cli.json", r#"{"keybinds":{"session_delete":"ctrl+a","session_delete":"ctrl+b"}}"#);
    let config = rt(service(&tmp, None).get());
    assert_eq!(config["keybinds"], json!({ "session.delete": "ctrl+b" }));
    assert_eq!(tmp.json("cli.json")["keybinds"], json!({ "session.delete": "ctrl+b" }));
}

#[test]
fn migrates_and_updates_the_effective_duplicate_top_level_keybinds() {
    let tmp = Tmp::new();
    tmp.write("cli.json", r#"{"keybinds":{"session_delete":"first"},"keybinds":{"session_delete":"last"}}"#);
    let svc = service(&tmp, None);
    assert_eq!(rt(svc.get())["keybinds"], json!({ "session.delete": "last" }));
    let config = rt(svc.update(|d| d["keybinds"]["session.delete"] = json!("changed"))).unwrap();
    assert_eq!(config["keybinds"], json!({ "session.delete": "changed" }));
    assert_eq!(tmp.json("cli.json")["keybinds"], json!({ "session.delete": "changed" }));
}

#[test]
fn updates_effective_duplicate_canonical_keybinds() {
    let tmp = Tmp::new();
    tmp.write(
        "cli.json",
        r#"{"keybinds":{"session.delete":"first","session.delete":"last","opencode.settings":"off","opencode.settings":"on"}}"#,
    );
    let svc = service(&tmp, None);
    assert_eq!(rt(svc.get())["keybinds"], json!({ "session.delete": "last", "opencode.settings": "on" }));
    let config = rt(svc.update(|d| {
        d["keybinds"]["session.delete"] = json!("changed");
        d["keybinds"]["opencode.settings"] = json!("changed");
    }))
    .unwrap();
    assert_eq!(config["keybinds"], json!({ "session.delete": "changed", "opencode.settings": "changed" }));
    assert_eq!(tmp.json("cli.json")["keybinds"], json!({ "session.delete": "changed", "opencode.settings": "changed" }));
}

#[test]
fn removes_orphaned_keybinds_without_deleting_trailing_comments() {
    let tmp = Tmp::new();
    tmp.write(
        "cli.json",
        "{\n  \"keybinds\": {\n    \"app_heap_snapshot\": \"ctrl+h\" /* Keep legacy explanation */,\n    \"app.heap_snapshot\": \"ctrl+shift+h\" /* Keep canonical explanation */,\n  },\n}\n",
    );
    let config = rt(service(&tmp, None).get());
    assert_eq!(config["keybinds"], json!({}));
    let text = tmp.read("cli.json");
    assert!(text.contains("/* Keep legacy explanation */"));
    assert!(text.contains("/* Keep canonical explanation */"));
    assert_eq!(parse_jsonc(&text).unwrap()["keybinds"], json!({}));
}

#[test]
fn updates_a_config_draft_while_preserving_jsonc_comments() {
    let tmp = Tmp::new();
    tmp.write("cli.json", "{\n  // Keep this comment\n  \"animations\": true\n}\n");
    let config = rt(service(&tmp, None).update(|d| {
        d["prompt"] = json!({ "paste": "compact" });
        d["mini"] = json!({ "thinking": "hide", "shell_output": "hide", "turn_summary": "hide", "splash": "hide", "work_spinner": "block-low-comet", "mono": true });
    }))
    .unwrap();
    assert_eq!(config, json!({
        "animations": true,
        "prompt": { "paste": "compact" },
        "mini": { "thinking": "hide", "shell_output": "hide", "turn_summary": "hide", "splash": "hide", "work_spinner": "block-low-comet", "mono": true },
    }));
    assert!(tmp.read("cli.json").contains("// Keep this comment"));
}

#[test]
fn invalid_field_ignores_the_whole_file_and_invalid_update_fails() {
    let tmp = Tmp::new();
    tmp.write("cli.json", r#"{"animations":true,"mouse":"yes"}"#);
    let svc = service(&tmp, None);
    assert_eq!(rt(svc.get()), json!({}));
    let err = rt(svc.update(|d| d["leader"] = json!({ "timeout": 0 }))).unwrap_err();
    assert_eq!(err.to_string(), "Failed to update CLI config");
    assert_eq!(err.cause, "Invalid CLI config update");
}

#[test]
fn resolve_applies_defaults() {
    let resolved = oc_config::cli::schema::resolve(&json!({ "tabs": { "enabled": true }, "cursor": {} }), false, None);
    assert_eq!(resolved["tabs"], json!({ "enabled": true, "mode": "on", "scope": "cwd", "layout": "horizontal", "indicators": "status" }));
    assert_eq!(resolved["leader"], json!({ "timeout": 2000 }));
    assert_eq!(resolved["mouse"], json!(true));
    assert_eq!(resolved["cursor"], json!({ "style": "block", "blinking": true }));
    assert_eq!(resolved["session"], json!({ "new_location": "launch", "permissions": "prompt", "tps": true }));
    assert_eq!(resolved["attention"]["volume"], json!(0.4));
    assert_eq!(resolved["keybinds"]["terminal.suspend"], json!("none"));
    assert!(resolved["keybinds"]["input.undo"].as_str().unwrap().starts_with("ctrl+z"));
    let auto = oc_config::cli::schema::resolve(&json!({}), true, Some("1"));
    assert_eq!(auto["tabs"]["enabled"], json!(false));
}
