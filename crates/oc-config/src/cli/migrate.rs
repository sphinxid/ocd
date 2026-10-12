//! `cli.json` migration (`packages/cli/src/config/migrate.ts`).

use std::path::{Path, PathBuf};

use serde_json::{Map, Value, json};

use super::schema::{SCHEMA_URL, decode};
use super::{keybind, v1};
use crate::jsonc_edit::{self, ModifyOptions, NodeId, ParseOptions, Segment, Tree};

/// Result of a migration that changed something.
#[derive(Debug, Clone, PartialEq)]
pub struct Migration {
    /// The migrated value (decoded for in-place edits, raw for v1 migration).
    pub info: Value,
    /// Persistence failure (the migrated value is still used in memory).
    pub cause: Option<String>,
}

/// JSONC parse with `allowTrailingComma: true`; any error → `None`.
pub fn parse_jsonc(text: &str) -> Option<Value> {
    let (value, errors) =
        jsonc_edit::parse(text, ParseOptions { allow_trailing_comma: true, ..ParseOptions::default() });
    if !errors.is_empty() {
        return None;
    }
    value
}

/// `parseRecord` / `readJson`: JSONC → record or `None`.
pub fn parse_record(text: &str) -> Option<Map<String, Value>> {
    parse_jsonc(text)?.as_object().cloned()
}

fn read_record(path: &Path) -> Option<Map<String, Value>> {
    parse_record(&std::fs::read_to_string(path).ok()?)
}

/// Write `text` to `<file>.<pid>.<uuid>.tmp` (0600) and rename over `file`; the temp file is
/// removed in all cases.
fn persist(file: &Path, text: &str) -> Option<String> {
    let temp = PathBuf::from(format!("{}.{}.{}.tmp", file.display(), std::process::id(), uuid::Uuid::new_v4()));
    let result = (|| -> std::io::Result<()> {
        if let Some(parent) = file.parent() {
            std::fs::create_dir_all(parent)?;
        }
        write_private(&temp, text)?;
        std::fs::rename(&temp, file)
    })();
    let _ = std::fs::remove_file(&temp);
    result.err().map(|e| e.to_string())
}

/// Write a file with mode 0600.
pub fn write_private(path: &Path, text: &str) -> std::io::Result<()> {
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    use std::io::Write;
    options.open(path)?.write_all(text.as_bytes())
}

fn modify(text: &str, path: &[&str], value: Option<&Value>) -> String {
    let segments: Vec<Segment> = path.iter().map(|s| Segment::from(*s)).collect();
    jsonc_edit::apply_modify(text, &segments, value, &ModifyOptions::formatted()).unwrap_or_else(|_| text.to_string())
}

fn find_keybind_objects(text: &str) -> (Option<Tree>, Vec<NodeId>) {
    let Some(tree) = jsonc_edit::parse_tree(text) else { return (None, vec![]) };
    let root = tree.node(tree.root);
    let found = root
        .children
        .iter()
        .copied()
        .filter(|id| {
            let n = tree.node(*id);
            n.children.first().and_then(|k| tree.node(*k).value.as_ref()) == Some(&Value::String("keybinds".into()))
        })
        .collect();
    (Some(tree), found)
}

fn find_keybind_properties(text: &str, name: &str) -> (Option<Tree>, Vec<NodeId>) {
    let (tree, objects) = find_keybind_objects(text);
    let Some(tree) = tree else { return (None, vec![]) };
    let Some(last) = objects.last() else { return (Some(tree), vec![]) };
    let Some(value) = tree.node(*last).children.get(1).copied() else { return (Some(tree), vec![]) };
    let props = tree
        .node(value)
        .children
        .iter()
        .copied()
        .filter(|id| {
            tree.node(*id).children.first().and_then(|k| tree.node(*k).value.as_ref())
                == Some(&Value::String(name.into()))
        })
        .collect();
    (Some(tree), props)
}

fn find_comma(text: &str, start: usize, end: usize) -> Option<usize> {
    let mut scanner = jsonc_edit::create_scanner(text, false);
    scanner.set_position(start);
    loop {
        scanner.scan();
        let offset = scanner.get_token_offset();
        if scanner.get_token_length() == 0 || offset >= end {
            return None;
        }
        if text.as_bytes().get(offset) == Some(&b',') {
            return Some(offset);
        }
    }
}

fn remove_property(text: &str, tree: &Tree, property: NodeId) -> String {
    let node = tree.node(property);
    let parent = node.parent.map(|p| tree.node(p));
    let siblings: &[NodeId] = parent.map(|p| p.children.as_slice()).unwrap_or(&[]);
    let index = siblings.iter().position(|id| *id == property).unwrap_or(0);
    let end = node.offset + node.length;
    if let Some(next) = siblings.get(index + 1)
        && let Some(comma) = find_comma(text, end, tree.node(*next).offset)
    {
        return format!("{}{}{}", &text[..node.offset], &text[end..comma], &text[comma + 1..]);
    }
    if index > 0
        && let Some(previous) = siblings.get(index - 1)
    {
        let p = tree.node(*previous);
        if let Some(comma) = find_comma(text, p.offset + p.length, node.offset) {
            return format!("{}{}{}", &text[..comma], &text[comma + 1..node.offset], &text[end..]);
        }
    }
    let parent_end = parent.map(|p| p.offset + p.length).unwrap_or(0);
    if let Some(comma) = find_comma(text, end, parent_end) {
        return format!("{}{}{}", &text[..node.offset], &text[end..comma], &text[comma + 1..]);
    }
    format!("{}{}", &text[..node.offset], &text[end..])
}

/// In-place migration of an existing `cli.json` text. Returns the updated text when it changed and
/// still decodes.
pub fn migrate_text(text: &str) -> Option<(String, Value)> {
    let config = parse_record(text)?;
    let terminal = config.get("terminal").and_then(Value::as_object);
    let legacy_copy = terminal.and_then(|t| t.get("copy_on_select")).and_then(Value::as_bool);
    let renamed = match legacy_copy {
        None => text.to_string(),
        Some(legacy) => {
            let mut out = text.to_string();
            if terminal.and_then(|t| t.get("copy")).is_none() {
                out = modify(&out, &["terminal", "copy"], Some(&json!(if legacy { "select" } else { "manual" })));
            }
            modify(&out, &["terminal", "copy_on_select"], None)
        }
    };
    let keybinds = config.get("keybinds").and_then(Value::as_object);
    let updated = match keybinds {
        None => renamed,
        Some(keybinds) => {
            // Remove every top-level keybinds object but the last.
            let mut text = renamed.clone();
            let count = find_keybind_objects(&renamed).1.len();
            for _ in 0..count.saturating_sub(1) {
                let (tree, objects) = find_keybind_objects(&text);
                if let (Some(tree), Some(first)) = (tree, objects.first()) {
                    text = remove_property(&text, &tree, *first);
                }
            }
            for name in keybinds.keys() {
                let target = v1::command_map(name).map(str::to_string).or_else(|| {
                    (keybind::is_definition(name) || v1::is_legacy_target(name)).then(|| name.clone())
                });
                let Some(target) = target else { continue };
                let (_, properties) = find_keybind_properties(&text, name);
                if properties.is_empty() {
                    continue;
                }
                let remove =
                    !keybind::is_definition(&target) || (&target != name && keybinds.contains_key(&target));
                let removals = if remove { properties.len() } else { properties.len() - 1 };
                let mut cleaned = text.clone();
                for _ in 0..removals {
                    let (tree, props) = find_keybind_properties(&cleaned, name);
                    if let (Some(tree), Some(first)) = (tree, props.first()) {
                        cleaned = remove_property(&cleaned, &tree, *first);
                    }
                }
                if remove || &target == name {
                    text = cleaned;
                    continue;
                }
                let (tree, props) = find_keybind_properties(&cleaned, name);
                let key = tree.as_ref().and_then(|t| props.first().and_then(|p| t.node(*p).children.first().map(|k| t.node(*k))));
                let Some(key) = key else { continue };
                text = format!(
                    "{}{}{}",
                    &cleaned[..key.offset],
                    serde_json::to_string(&target).unwrap_or_default(),
                    &cleaned[key.offset + key.length..]
                );
            }
            text
        }
    };
    if updated == text {
        return None;
    }
    let info = decode(&parse_jsonc(&updated)?)?;
    Some((updated, info))
}

fn truthy(v: &Value) -> bool {
    match v {
        Value::Null => false,
        Value::Bool(b) => *b,
        Value::Number(n) => n.as_f64().is_some_and(|f| f != 0.0 && !f.is_nan()),
        Value::String(s) => !s.is_empty(),
        _ => true,
    }
}

/// `a ?? b`
fn nullish<'a>(a: Option<&'a Value>, b: Option<&'a Value>) -> Option<&'a Value> {
    match a {
        Some(v) if !v.is_null() => Some(v),
        _ => b,
    }
}

/// `migrateV1(legacy, kv)`: legacy `tui.json` + `kv.json` → v2 CLI values (not decoded).
pub fn migrate_v1(legacy: Option<&v1::TuiConfigV1>, kv: &Map<String, Value>) -> Map<String, Value> {
    let mut plugins: Vec<Value> = Vec::new();
    if let Some(list) = legacy.and_then(|l| l.plugin.as_ref()) {
        for p in list {
            plugins.push(match p {
                v1::PluginSpec::Name(n) => json!(n),
                v1::PluginSpec::Tuple(n, o) => json!({ "package": n, "options": o }),
            });
        }
    }
    if let Some(enabled) = legacy.and_then(|l| l.plugin_enabled.as_ref()) {
        for (id, on) in enabled {
            plugins.push(json!(if *on { id.clone() } else { format!("-{id}") }));
        }
    }
    let legacy_theme = legacy.and_then(|l| l.theme.clone()).map(Value::String);
    let theme_name = nullish(legacy_theme.as_ref(), kv.get("theme")).cloned();
    let theme_mode = kv.get("theme_mode_lock").cloned();
    let sound_pack = kv.get("attention_sound_pack").cloned();
    let stacked = legacy.and_then(|l| l.diff_style.as_deref()) == Some("stacked");
    let stacked_view = stacked.then(|| json!("unified"));
    let diff_view = nullish(kv.get("diff_viewer_view"), stacked_view.as_ref()).cloned();
    let thinking_visibility = kv.get("thinking_visibility").map(|v| json!(if truthy(v) { "show" } else { "hide" }));
    let thinking = nullish(kv.get("thinking_mode"), thinking_visibility.as_ref()).cloned();
    let keybinds = legacy.and_then(|l| l.keybinds.as_ref()).map(|k| {
        let mut out = Map::new();
        for (name, value) in k {
            let target = v1::command_map(name).map(str::to_string).unwrap_or_else(|| name.clone());
            if keybind::is_definition(&target) {
                out.insert(target, value.clone());
            }
        }
        out
    });

    let mut out = Map::new();
    if theme_name.is_some() || theme_mode.is_some() {
        let mut theme = Map::new();
        if let Some(n) = theme_name {
            theme.insert("name".into(), n);
        }
        if let Some(m) = theme_mode {
            theme.insert("mode".into(), m);
        }
        out.insert("theme".into(), Value::Object(theme));
    }
    if let Some(k) = keybinds {
        out.insert("keybinds".into(), Value::Object(k));
    }
    if !plugins.is_empty() {
        out.insert("plugins".into(), Value::Array(plugins));
    }
    if let Some(t) = legacy.and_then(|l| l.leader_timeout.clone()) {
        out.insert("leader".into(), json!({ "timeout": t }));
    }
    let speed = legacy.and_then(|l| l.scroll_speed.clone());
    let acceleration = legacy.and_then(|l| l.scroll_acceleration).map(|a| a.enabled);
    if speed.is_some() || acceleration.is_some() {
        let mut scroll = Map::new();
        if let Some(s) = speed {
            scroll.insert("speed".into(), Value::Number(s));
        }
        if let Some(a) = acceleration {
            scroll.insert("acceleration".into(), Value::Bool(a));
        }
        out.insert("scroll".into(), Value::Object(scroll));
    }
    let attention = legacy.and_then(|l| l.attention.as_ref());
    if attention.is_some() || sound_pack.is_some() {
        let mut a: Map<String, Value> = attention.map(|a| a.iter().map(|(k, v)| (k.clone(), v.clone())).collect()).unwrap_or_default();
        if let Some(p) = sound_pack {
            a.insert("sound_pack".into(), p);
        }
        out.insert("attention".into(), Value::Object(a));
    }
    let diff_style = legacy.and_then(|l| l.diff_style.as_ref());
    if diff_style.is_some()
        || kv.contains_key("diff_wrap_mode")
        || kv.contains_key("diff_viewer_show_file_tree")
        || kv.contains_key("diff_viewer_single_patch")
        || diff_view.is_some()
    {
        let mut diffs = Map::new();
        for (from, to) in [("diff_wrap_mode", "wrap"), ("diff_viewer_show_file_tree", "tree"), ("diff_viewer_single_patch", "single")] {
            if let Some(v) = kv.get(from) {
                diffs.insert(to.into(), v.clone());
            }
        }
        if let Some(v) = diff_view {
            diffs.insert("view".into(), v);
        }
        out.insert("diffs".into(), Value::Object(diffs));
    }
    if let Some(v) = kv.get("terminal_title_enabled") {
        out.insert("terminal".into(), json!({ "title": v }));
    }
    if kv.contains_key("file_context_enabled") || kv.contains_key("paste_summary_enabled") {
        let mut prompt = Map::new();
        if let Some(v) = kv.get("file_context_enabled") {
            prompt.insert("editor".into(), v.clone());
        }
        if let Some(v) = kv.get("paste_summary_enabled") {
            prompt.insert("paste".into(), json!(if truthy(v) { "compact" } else { "full" }));
        }
        out.insert("prompt".into(), Value::Object(prompt));
    }
    if kv.contains_key("sidebar") || kv.contains_key("scrollbar_visible") || thinking.is_some() || kv.contains_key("exploration_grouping") {
        let mut session = Map::new();
        if let Some(v) = kv.get("sidebar") {
            session.insert("sidebar".into(), v.clone());
        }
        if let Some(v) = kv.get("scrollbar_visible") {
            session.insert("scrollbar".into(), v.clone());
        }
        if let Some(v) = thinking {
            session.insert("thinking".into(), v);
        }
        if let Some(v) = kv.get("exploration_grouping") {
            session.insert("grouping".into(), json!(if truthy(v) { "auto" } else { "none" }));
        }
        out.insert("session".into(), Value::Object(session));
    }
    if let Some(v) = kv.get("animations_enabled") {
        out.insert("animations".into(), v.clone());
    }
    if let Some(m) = legacy.and_then(|l| l.mouse) {
        out.insert("mouse".into(), Value::Bool(m));
    }
    if let Some(c) = legacy.and_then(|l| l.cursor.clone()) {
        out.insert("cursor".into(), c);
    }
    out
}

/// `ConfigMigration.run({ file, config, state })`. `Err` is a read failure of an existing file.
pub fn run(file: &Path, config_dir: &Path, state_dir: &Path) -> Result<Option<Migration>, String> {
    if file.exists() {
        let text = std::fs::read_to_string(file).map_err(|e| e.to_string())?;
        let Some((updated, info)) = migrate_text(&text) else { return Ok(None) };
        let cause = persist(file, &updated);
        return Ok(Some(Migration { info, cause }));
    }
    let tui = config_dir.join("tui.json");
    let kv_path = state_dir.join("kv.json");
    let legacy_value = read_record(&tui);
    let legacy = legacy_value.as_ref().and_then(|v| v1::decode_tui_v1(&Value::Object(v.clone())));
    let kv = read_record(&kv_path);
    let values = migrate_v1(legacy.as_ref(), kv.as_ref().unwrap_or(&Map::new()));
    if values.is_empty() {
        return Ok(None);
    }
    let mut migrated = Map::new();
    migrated.insert("$schema".into(), json!(SCHEMA_URL));
    migrated.extend(values);
    let migrated = Value::Object(migrated);
    let text = serde_json::to_string_pretty(&migrated).unwrap_or_default() + "\n";
    let cause = persist(file, &text);
    if cause.is_none() {
        let from: Vec<String> = [legacy_value.as_ref().map(|_| &tui), kv.as_ref().map(|_| &kv_path)]
            .into_iter()
            .flatten()
            .map(|p| p.display().to_string())
            .collect();
        tracing::info!(from = ?from, to = %file.display(), "migrated cli config");
    }
    Ok(Some(Migration { info: migrated, cause }))
}
