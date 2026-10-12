//! TUI client configuration: `<config>/cli.json` (JSONC) with resolved defaults (spec §7.2).

use serde_json::{Map, Value};
use std::path::PathBuf;

pub const SCHEMA_URL: &str = "https://opencode.ai/v2/cli.json";

#[derive(Clone, Debug)]
pub struct Config {
    pub raw: Value,
    pub path: PathBuf,
    /// Whether terminal suspend (ctrl+z) is supported on this platform.
    pub terminal_suspend: bool,
}

/// Strip `//` and `/* */` comments and trailing commas from JSONC text.
pub fn strip_jsonc(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let chars: Vec<char> = text.chars().collect();
    let mut i = 0;
    let mut in_str = false;
    while i < chars.len() {
        let c = chars[i];
        if in_str {
            out.push(c);
            if c == '\\' && i + 1 < chars.len() {
                out.push(chars[i + 1]);
                i += 2;
                continue;
            }
            if c == '"' {
                in_str = false;
            }
            i += 1;
            continue;
        }
        if c == '"' {
            in_str = true;
            out.push(c);
            i += 1;
            continue;
        }
        if c == '/' && i + 1 < chars.len() && chars[i + 1] == '/' {
            while i < chars.len() && chars[i] != '\n' {
                i += 1;
            }
            continue;
        }
        if c == '/' && i + 1 < chars.len() && chars[i + 1] == '*' {
            i += 2;
            while i + 1 < chars.len() && !(chars[i] == '*' && chars[i + 1] == '/') {
                i += 1;
            }
            i += 2;
            continue;
        }
        if c == ',' {
            // Drop trailing commas before `}` or `]`.
            let mut j = i + 1;
            while j < chars.len() && chars[j].is_whitespace() {
                j += 1;
            }
            if j < chars.len() && (chars[j] == '}' || chars[j] == ']') {
                i += 1;
                continue;
            }
        }
        out.push(c);
        i += 1;
    }
    out
}

pub fn parse_jsonc(text: &str) -> Option<Value> {
    serde_json::from_str(&strip_jsonc(text)).ok()
}

impl Config {
    pub fn path() -> PathBuf {
        crate::util::paths::config_dir().join("cli.json")
    }

    pub fn load() -> Config {
        let path = Self::path();
        let mut raw = std::fs::read_to_string(&path)
            .ok()
            .and_then(|t| parse_jsonc(&t))
            .filter(|v| v.is_object())
            .unwrap_or_else(|| Value::Object(Map::new()));
        if let Ok(content) = std::env::var("OPENCODE_CLI_CONFIG_CONTENT")
            && let Some(extra) = parse_jsonc(&content)
        {
            raw = crate::theme::resolve::merge(&raw, &extra);
        }
        Config { raw, path, terminal_suspend: !cfg!(windows) }
    }

    pub fn from_value(raw: Value) -> Config {
        Config { raw, path: PathBuf::from("/nonexistent/cli.json"), terminal_suspend: !cfg!(windows) }
    }

    pub fn get(&self, path: &str) -> Option<&Value> {
        let mut cur = &self.raw;
        for k in path.split('.') {
            cur = cur.get(k)?;
        }
        if cur.is_null() { None } else { Some(cur) }
    }

    pub fn str(&self, path: &str) -> Option<&str> {
        self.get(path).and_then(|v| v.as_str())
    }

    pub fn bool(&self, path: &str) -> Option<bool> {
        self.get(path).and_then(|v| v.as_bool())
    }

    pub fn num(&self, path: &str) -> Option<f64> {
        self.get(path).and_then(|v| v.as_f64())
    }

    /// Set a value at a dotted path (None removes it) and persist atomically.
    pub fn set(&mut self, path: &str, value: Option<Value>) {
        fn set_in(obj: &mut Value, keys: &[&str], value: Option<Value>) {
            if !obj.is_object() {
                *obj = Value::Object(Map::new());
            }
            let map = obj.as_object_mut().unwrap();
            if keys.len() == 1 {
                match value {
                    Some(v) => {
                        map.insert(keys[0].to_string(), v);
                    }
                    None => {
                        map.remove(keys[0]);
                    }
                }
                return;
            }
            let entry = map.entry(keys[0].to_string()).or_insert_with(|| Value::Object(Map::new()));
            set_in(entry, &keys[1..], value);
        }
        let keys: Vec<&str> = path.split('.').collect();
        let before = self.get(path).cloned();
        if before == value {
            return;
        }
        set_in(&mut self.raw, &keys, value.clone());
        self.persist(path, value);
    }

    fn persist(&self, path: &str, value: Option<Value>) {
        if self.path.starts_with("/nonexistent") {
            return;
        }
        // Re-read the file so concurrent edits by other processes (and comments) are kept.
        let existing = std::fs::read_to_string(&self.path).ok();
        let text = edit_config_text(existing.as_deref(), path, value.as_ref());
        let _ = crate::util::paths::write_atomic(&self.path, text.as_bytes());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(&self.path, std::fs::Permissions::from_mode(0o600));
        }
    }

    // ---- resolved accessors -------------------------------------------------

    pub fn theme_name(&self) -> Option<&str> {
        self.str("theme.name")
    }
    pub fn theme_mode(&self) -> &str {
        self.str("theme.mode").unwrap_or("system")
    }
    pub fn leader_timeout(&self) -> u64 {
        self.num("leader.timeout").filter(|v| *v > 0.0).map(|v| v as u64).unwrap_or(2000)
    }
    pub fn animations(&self) -> bool {
        self.bool("animations").unwrap_or(true)
    }
    pub fn mouse(&self) -> bool {
        self.bool("mouse").unwrap_or(true)
    }
    pub fn scroll_speed(&self) -> f64 {
        self.num("scroll.speed").filter(|v| *v >= 0.001).unwrap_or(3.0)
    }
    pub fn scroll_acceleration(&self) -> bool {
        self.bool("scroll.acceleration").unwrap_or(false)
    }
    pub fn notifications(&self) -> bool {
        self.bool("attention.notifications").unwrap_or(false)
    }
    pub fn sound(&self) -> bool {
        self.bool("attention.sound").unwrap_or(false)
    }
    pub fn volume(&self) -> f64 {
        self.num("attention.volume").unwrap_or(0.4)
    }
    pub fn diffs_view(&self) -> &str {
        self.str("diffs.view").unwrap_or("auto")
    }
    pub fn diffs_wrap(&self) -> &str {
        self.str("diffs.wrap").unwrap_or("word")
    }
    pub fn terminal_title(&self) -> bool {
        self.bool("terminal.title").unwrap_or(true)
    }
    pub fn copy_mode(&self) -> &str {
        self.str("terminal.copy").unwrap_or(if cfg!(windows) { "manual" } else { "select" })
    }
    pub fn prompt_editor(&self) -> bool {
        self.bool("prompt.editor").unwrap_or(true)
    }
    pub fn prompt_paste(&self) -> &str {
        self.str("prompt.paste").unwrap_or("compact")
    }
    pub fn session_sidebar(&self) -> &str {
        self.str("session.sidebar").unwrap_or("auto")
    }
    pub fn session_scrollbar(&self) -> bool {
        self.bool("session.scrollbar").unwrap_or(false)
    }
    pub fn thinking(&self) -> &str {
        self.str("session.thinking").unwrap_or("hide")
    }
    pub fn grouping(&self) -> &str {
        self.str("session.grouping").unwrap_or("auto")
    }
    pub fn verbosity(&self) -> &str {
        self.str("session.verbosity").unwrap_or("medium")
    }
    pub fn tps(&self) -> bool {
        self.bool("session.tps").unwrap_or(true)
    }
    pub fn markdown_mode(&self) -> &str {
        self.str("session.markdown").unwrap_or("rendered")
    }
    pub fn new_location(&self) -> &str {
        self.str("session.new_location").unwrap_or("launch")
    }
    pub fn permissions_mode(&self) -> &str {
        self.str("session.permissions").unwrap_or("prompt")
    }
    pub fn tabs_mode(&self) -> &str {
        if let Some(m) = self.str("tabs.mode") {
            return m;
        }
        match self.bool("tabs.enabled") {
            None => "auto",
            Some(true) => "on",
            Some(false) => "off",
        }
    }
    pub fn tabs_enabled(&self) -> bool {
        let m = self.tabs_mode();
        m == "on" || (m == "auto" && std::env::var("HERDR_ENV").ok().as_deref() != Some("1"))
    }
    pub fn tabs_scope(&self) -> &str {
        self.str("tabs.scope").unwrap_or("cwd")
    }
    pub fn tabs_layout(&self) -> &str {
        self.str("tabs.layout").unwrap_or("horizontal")
    }
    pub fn tabs_indicators(&self) -> &str {
        self.str("tabs.indicators").unwrap_or("status")
    }
    pub fn devtools(&self) -> Option<bool> {
        self.bool("debug.devtools")
    }
    pub fn turn_tokens(&self) -> Option<&Value> {
        self.get("debug.turn_tokens").filter(|v| v.as_bool() != Some(false))
    }

    pub fn keybinds(&self) -> crate::keymap::Keybinds {
        crate::keymap::Keybinds::new(self.get("keybinds"), self.leader_timeout(), self.terminal_suspend)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn jsonc() {
        let v = parse_jsonc("{\n // c\n \"a\": 1, /* x */ \"b\": \"//no\",\n}").unwrap();
        assert_eq!(v["a"], 1);
        assert_eq!(v["b"], "//no");
    }

    #[test]
    fn defaults() {
        let c = Config::from_value(serde_json::json!({"tabs": {"enabled": false}}));
        assert_eq!(c.leader_timeout(), 2000);
        assert_eq!(c.tabs_mode(), "off");
        assert!(!c.tabs_enabled());
        assert_eq!(c.thinking(), "hide");
        assert_eq!(c.verbosity(), "medium");
        assert!(c.tps());
        assert_eq!(c.volume(), 0.4);
    }

    #[test]
    fn set_nested() {
        let mut c = Config::from_value(serde_json::json!({}));
        c.set("theme.name", Some("nord".into()));
        assert_eq!(c.theme_name(), Some("nord"));
        c.set("theme.name", None);
        assert_eq!(c.theme_name(), None);
    }
}

/// Apply one `path = value` change (`None` removes) to the text of `cli.json`, preserving
/// comments and formatting with a jsonc-parser `modify` edit. Falls back to a pretty-printed
/// rewrite when the existing text cannot be edited.
pub fn edit_config_text(existing: Option<&str>, path: &str, value: Option<&Value>) -> String {
    use crate::util::jsonc_edit::{ModifyOptions, Segment, apply_modify};
    let segments: Vec<Segment> = path.split('.').map(Segment::from).collect();
    let base = match existing {
        Some(t) if !t.trim().is_empty() && parse_jsonc(t).is_some_and(|v| v.is_object()) => t.to_string(),
        _ => {
            let mut t = serde_json::to_string_pretty(&serde_json::json!({ "$schema": SCHEMA_URL })).unwrap_or_default();
            t.push('\n');
            t
        }
    };
    if let Ok(t) = apply_modify(&base, &segments, value, &ModifyOptions::formatted()) {
        return t;
    }
    let mut on_disk = parse_jsonc(&base).unwrap_or_else(|| serde_json::json!({ "$schema": SCHEMA_URL }));
    let keys: Vec<&str> = path.split('.').collect();
    let mut cur = &mut on_disk;
    for (i, k) in keys.iter().enumerate() {
        if !cur.is_object() {
            *cur = Value::Object(Map::new());
        }
        let map = cur.as_object_mut().unwrap();
        if i == keys.len() - 1 {
            match value {
                Some(v) => {
                    map.insert(k.to_string(), v.clone());
                }
                None => {
                    map.remove(*k);
                }
            }
            break;
        }
        cur = map.entry(k.to_string()).or_insert_with(|| Value::Object(Map::new()));
    }
    let mut text = serde_json::to_string_pretty(&on_disk).unwrap_or_default();
    text.push('\n');
    text
}

#[cfg(test)]
mod edit_tests {
    use super::*;

    #[test]
    fn comment_preserving_writes() {
        let src = "{\n  // my theme\n  \"theme\": { \"name\": \"nord\" }, /* keep */\n  \"animations\": true\n}\n";
        let out = edit_config_text(Some(src), "theme.name", Some(&serde_json::json!("dracula")));
        assert_eq!(out, src.replace("nord", "dracula"));
        let out = edit_config_text(Some(&out), "attention.sound", Some(&serde_json::json!(true)));
        assert!(out.contains("// my theme") && out.contains("/* keep */"), "{out}");
        assert_eq!(parse_jsonc(&out).unwrap()["attention"]["sound"], serde_json::json!(true));
        let out = edit_config_text(Some(&out), "animations", None);
        assert!(!out.contains("animations") && out.contains("// my theme"), "{out}");
        let fresh = edit_config_text(None, "theme.mode", Some(&serde_json::json!("dark")));
        let v = parse_jsonc(&fresh).unwrap();
        assert_eq!(v["$schema"], serde_json::json!(SCHEMA_URL));
        assert_eq!(v["theme"]["mode"], serde_json::json!("dark"));
    }
}
