//! `cli.json` schema (`packages/cli/src/config/schema.ts`, `packages/tui/src/config/index.tsx`).
//!
//! Decoding is whole-file: any invalid declared field fails the whole value. Decoded values are
//! canonical JSON (struct keys in schema order, unknown keys stripped).

use std::sync::LazyLock;

use serde_json::{Map, Value};

use super::{keybind, v1};

/// `SchemaURL`
pub const SCHEMA_URL: &str = "https://opencode.ai/v2/cli.json";

/// `MiniWorkSpinner` literals (default `block-soft-slide`).
pub const WORK_SPINNERS: [&str; 14] = [
    "block-soft-slide",
    "block-soft-sweep",
    "block-low-comet",
    "block-low-duet",
    "block-shuttle",
    "block-bridge",
    "block-squeeze",
    "small-toggle",
    "square-toggle",
    "grow-shrink",
    "quadrant-orbit",
    "crosshatch",
    "density-wave",
    "seed",
];

/// Field type of the declarative schema.
pub enum Spec {
    Str,
    Bool,
    /// `Schema.Int` with `> 0`.
    PositiveInt,
    /// `Schema.Finite` with inclusive bounds.
    Finite(Option<f64>, Option<f64>),
    Lit(&'static [&'static str]),
    Struct(Vec<(&'static str, Spec)>),
    Array(Box<Spec>),
    /// `Record(String, spec)`
    Record(Box<Spec>),
    Union(Vec<Spec>),
    Any,
    Custom(fn(&Value) -> Option<Value>),
    /// A required struct field.
    Required(Box<Spec>),
}

impl Spec {
    pub fn decode(&self, value: &Value) -> Option<Value> {
        match self {
            Spec::Str => value.is_string().then(|| value.clone()),
            Spec::Bool => value.is_boolean().then(|| value.clone()),
            Spec::PositiveInt => {
                let i = crate::decode::int(value)?;
                (i > 0).then(|| Value::from(i))
            }
            Spec::Finite(min, max) => {
                let n = value.as_number()?;
                let f = n.as_f64()?;
                if min.is_some_and(|m| f < m) || max.is_some_and(|m| f > m) {
                    return None;
                }
                Some(Value::Number(crate::decode::js_number(n)))
            }
            Spec::Lit(allowed) => allowed.contains(&value.as_str()?).then(|| value.clone()),
            Spec::Struct(fields) => {
                let o = value.as_object()?;
                let mut out = Map::new();
                for (name, spec) in fields {
                    match o.get(*name) {
                        Some(v) => {
                            out.insert((*name).to_string(), spec.decode(v)?);
                        }
                        None if matches!(spec, Spec::Required(_)) => return None,
                        None => {}
                    }
                }
                Some(Value::Object(out))
            }
            Spec::Array(item) => {
                Some(Value::Array(value.as_array()?.iter().map(|v| item.decode(v)).collect::<Option<Vec<_>>>()?))
            }
            Spec::Record(item) => Some(Value::Object(
                value
                    .as_object()?
                    .iter()
                    .map(|(k, v)| Some((k.clone(), item.decode(v)?)))
                    .collect::<Option<Map<_, _>>>()?,
            )),
            Spec::Union(members) => members.iter().find_map(|m| m.decode(value)),
            Spec::Any => Some(value.clone()),
            Spec::Custom(f) => f(value),
            Spec::Required(inner) => inner.decode(value),
        }
    }
}

fn s(fields: Vec<(&'static str, Spec)>) -> Spec {
    Spec::Struct(fields)
}

const SHOW_HIDE: &[&str] = &["show", "hide"];

fn keybinds(value: &Value) -> Option<Value> {
    keybind::decode_overrides(value).map(|m| Value::Object(m.into_iter().collect()))
}

/// `Config.Info` of the CLI (`{ $schema?, ...TuiConfig.Info.fields }`).
pub static INFO: LazyLock<Spec> = LazyLock::new(|| {
    s(vec![
        ("$schema", Spec::Str),
        ("theme", s(vec![("name", Spec::Str), ("mode", Spec::Lit(&["system", "dark", "light"]))])),
        ("keybinds", Spec::Custom(keybinds)),
        (
            "plugins",
            Spec::Array(Box::new(Spec::Union(vec![
                Spec::Str,
                s(vec![("package", Spec::Required(Box::new(Spec::Str))), ("options", Spec::Record(Box::new(Spec::Any)))]),
            ]))),
        ),
        ("leader", s(vec![("timeout", Spec::PositiveInt)])),
        ("scroll", s(vec![("speed", Spec::Finite(Some(0.001), None)), ("acceleration", Spec::Bool)])),
        (
            "attention",
            s(vec![
                ("notifications", Spec::Bool),
                ("sound", Spec::Bool),
                ("volume", Spec::Finite(Some(0.0), Some(1.0))),
                ("sound_pack", Spec::Str),
                ("sounds", Spec::Custom(v1::decode_sounds)),
            ]),
        ),
        (
            "diffs",
            s(vec![
                ("source", Spec::Lit(&["working", "branch", "committed", "turn"])),
                ("wrap", Spec::Lit(&["word", "none"])),
                ("tree", Spec::Bool),
                ("single", Spec::Bool),
                ("view", Spec::Lit(&["auto", "split", "unified"])),
            ]),
        ),
        ("terminal", s(vec![("title", Spec::Bool), ("copy", Spec::Lit(&["manual", "select"]))])),
        (
            "prompt",
            s(vec![("editor", Spec::Bool), ("paste", Spec::Lit(&["compact", "full"])), ("image_preview", Spec::Bool)]),
        ),
        (
            "session",
            s(vec![
                ("sidebar", Spec::Lit(&["auto", "hide"])),
                ("scrollbar", Spec::Bool),
                ("thinking", Spec::Lit(SHOW_HIDE)),
                ("grouping", Spec::Lit(&["auto", "none"])),
                ("verbosity", Spec::Lit(&["low", "medium", "high"])),
                ("image_preview", Spec::Bool),
                ("tps", Spec::Bool),
                ("markdown", Spec::Lit(&["source", "rendered"])),
                ("new_location", Spec::Lit(&["launch", "inherit"])),
                ("permissions", Spec::Lit(&["prompt", "autoaccept"])),
            ]),
        ),
        (
            "tabs",
            s(vec![
                ("mode", Spec::Lit(&["auto", "on", "off"])),
                ("enabled", Spec::Bool),
                ("scope", Spec::Lit(&["global", "cwd"])),
                ("layout", Spec::Lit(&["horizontal", "vertical"])),
                ("indicators", Spec::Lit(&["status", "numbers"])),
            ]),
        ),
        (
            "mini",
            s(vec![
                ("thinking", Spec::Lit(SHOW_HIDE)),
                ("tools", Spec::Lit(SHOW_HIDE)),
                ("shell_output", Spec::Lit(SHOW_HIDE)),
                ("turn_summary", Spec::Lit(SHOW_HIDE)),
                ("footer", Spec::Lit(SHOW_HIDE)),
                ("splash", Spec::Lit(SHOW_HIDE)),
                ("work_spinner", Spec::Lit(&WORK_SPINNERS)),
                ("mono", Spec::Bool),
                ("replay", Spec::Bool),
                ("replay_limit", Spec::PositiveInt),
            ]),
        ),
        (
            "debug",
            s(vec![
                ("devtools", Spec::Bool),
                ("timing", Spec::Bool),
                ("turn_tokens", Spec::Union(vec![Spec::Bool, Spec::Lit(&["verbose"])])),
            ]),
        ),
        ("experimental", Spec::Record(Box::new(Spec::Bool))),
        ("animations", Spec::Bool),
        ("mouse", Spec::Bool),
        ("cursor", Spec::Custom(v1::decode_cursor)),
    ])
});

/// Decode a CLI config value (`Schema.decodeUnknownOption(Info)`).
pub fn decode(value: &Value) -> Option<Value> {
    INFO.decode(value)
}

/// `normalizeLegacyTabs`: `tabs.enabled` → `tabs.mode ??= enabled ? "on" : "off"`, drop `enabled`.
pub fn normalize_legacy_tabs(info: &Value) -> Value {
    let mut info = info.clone();
    if let Some(tabs) = info.get_mut("tabs").and_then(Value::as_object_mut)
        && let Some(enabled) = tabs.get("enabled").cloned()
    {
        if !tabs.contains_key("mode") {
            let on = enabled.as_bool().unwrap_or(false);
            tabs.insert("mode".into(), Value::String(if on { "on" } else { "off" }.into()));
        }
        tabs.shift_remove("enabled");
    }
    info
}

/// Resolved TUI configuration (`TuiConfig.resolve`), without the keybind lookup structure.
pub fn resolve(info: &Value, terminal_suspend: bool, herdr_env: Option<&str>) -> Value {
    let mut out = info.as_object().cloned().unwrap_or_default();
    let get = |path: &[&str]| -> Option<&Value> {
        let mut v = info;
        for p in path {
            v = v.get(*p)?;
        }
        Some(v)
    };
    let tabs_mode = get(&["tabs", "mode"]).and_then(Value::as_str).map(str::to_string).unwrap_or_else(|| {
        match get(&["tabs", "enabled"]).and_then(Value::as_bool) {
            None => "auto".into(),
            Some(true) => "on".into(),
            Some(false) => "off".into(),
        }
    });
    let mut keybinds = info.get("keybinds").and_then(Value::as_object).cloned().unwrap_or_default();
    if !terminal_suspend {
        keybinds.insert("terminal.suspend".into(), Value::String("none".into()));
        if !keybinds.contains_key("input.undo") {
            let default = keybind::default_value("input.undo");
            let mut parts = vec!["ctrl+z".to_string()];
            if let Some(Value::String(d)) = default {
                parts.extend(d.split(',').map(str::to_string));
            }
            let mut unique: Vec<String> = Vec::new();
            for p in parts {
                if !unique.contains(&p) {
                    unique.push(p);
                }
            }
            keybinds.insert("input.undo".into(), Value::String(unique.join(",")));
        }
    }
    out.insert(
        "attention".into(),
        serde_json::json!({
            "notifications": get(&["attention", "notifications"]).cloned().unwrap_or(Value::Bool(false)),
            "sound": get(&["attention", "sound"]).cloned().unwrap_or(Value::Bool(false)),
            "volume": get(&["attention", "volume"]).cloned().unwrap_or(serde_json::json!(0.4)),
            "sound_pack": get(&["attention", "sound_pack"]).cloned().unwrap_or(Value::String("opencode.default".into())),
            "sounds": get(&["attention", "sounds"]).cloned().unwrap_or(serde_json::json!({})),
        }),
    );
    out.insert("keybinds".into(), Value::Object(keybinds));
    out.insert(
        "leader".into(),
        serde_json::json!({ "timeout": get(&["leader", "timeout"]).cloned().unwrap_or(serde_json::json!(2000)) }),
    );
    out.insert("mouse".into(), get(&["mouse"]).cloned().unwrap_or(Value::Bool(true)));
    match info.get("cursor") {
        Some(cursor) if cursor.is_object() => {
            out.insert(
                "cursor".into(),
                serde_json::json!({
                    "style": cursor.get("style").cloned().unwrap_or(Value::String("block".into())),
                    "blinking": cursor.get("blinking").cloned().unwrap_or(Value::Bool(true)),
                }),
            );
        }
        _ => {
            out.shift_remove("cursor");
        }
    }
    let mut session = info.get("session").and_then(Value::as_object).cloned().unwrap_or_default();
    for (key, default) in [("new_location", "launch"), ("permissions", "prompt")] {
        if !session.contains_key(key) {
            session.insert(key.into(), Value::String(default.into()));
        }
    }
    if !session.contains_key("tps") {
        session.insert("tps".into(), Value::Bool(true));
    }
    out.insert("session".into(), Value::Object(session));
    let mut tabs = info.get("tabs").and_then(Value::as_object).cloned().unwrap_or_default();
    let enabled = tabs_mode == "on" || (tabs_mode == "auto" && herdr_env != Some("1"));
    tabs.insert("mode".into(), Value::String(tabs_mode));
    tabs.insert("enabled".into(), Value::Bool(enabled));
    for (key, default) in [("scope", "cwd"), ("layout", "horizontal"), ("indicators", "status")] {
        if !tabs.contains_key(key) {
            tabs.insert(key.into(), Value::String(default.into()));
        }
    }
    out.insert("tabs".into(), Value::Object(tabs));
    Value::Object(out)
}
