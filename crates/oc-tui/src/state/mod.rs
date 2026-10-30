//! TUI-local persistent state, file-compatible with upstream (spec §7.3):
//! `<state>/model.json`, `session.json`, `prompt-history.jsonl`, `prompt-stash.jsonl`,
//! `frecency.jsonl`, and the keyed store `<state>/<channel>/tui/<key>.json`.

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use std::path::{Path, PathBuf};

pub const MAX_HISTORY: usize = 50;
pub const MAX_STASH: usize = 50;
pub const MAX_FRECENCY: usize = 1000;

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct PastedSource {
    pub start: f64,
    pub end: f64,
    pub text: String,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct PastedText {
    pub text: String,
    pub source: PastedSource,
}

/// `expandPastedTextPlaceholders` (`src/prompt/part.ts`): put each pasted text back in place of its
/// `[Pasted ~N lines]` placeholder, in document order, matching literally from where the previous one ended.
pub fn expand_pasted_text_placeholders(text: &str, pasted: &[PastedText]) -> String {
    let mut parts: Vec<&PastedText> = pasted.iter().collect();
    // `toSorted` is stable.
    parts.sort_by(|a, b| a.source.start.partial_cmp(&b.source.start).unwrap_or(std::cmp::Ordering::Equal));
    let mut cursor = 0;
    let mut result = String::new();
    for part in parts {
        let Some(found) = text[cursor..].find(&part.source.text) else { continue };
        let index = cursor + found;
        result.push_str(&text[cursor..index]);
        result.push_str(&part.text);
        cursor = index + part.source.text.len();
    }
    result.push_str(&text[cursor..]);
    result
}

/// `PromptInfo` (`src/prompt/history.tsx`).
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct PromptInfo {
    pub text: String,
    #[serde(default)]
    pub files: Vec<Value>,
    #[serde(default)]
    pub agents: Vec<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub skills: Option<Vec<Value>>,
    pub pasted: Vec<PastedText>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mode: Option<String>,
}

impl PromptInfo {
    pub fn empty() -> PromptInfo {
        PromptInfo { skills: Some(vec![]), ..Default::default() }
    }
    pub fn parse(v: &Value) -> Option<PromptInfo> {
        if !v.get("text").is_some_and(|t| t.is_string()) || !v.get("pasted").is_some_and(|p| p.is_array()) {
            return None;
        }
        serde_json::from_value(v.clone()).ok()
    }
}

fn read_lines(path: &Path) -> Vec<Value> {
    std::fs::read_to_string(path)
        .unwrap_or_default()
        .split('\n')
        .filter(|l| !l.is_empty())
        .filter_map(|l| serde_json::from_str(l).ok())
        .collect()
}

fn write_lines<T: Serialize>(path: &Path, items: &[T]) {
    let mut text = String::new();
    for i in items {
        text.push_str(&serde_json::to_string(i).unwrap_or_default());
        text.push('\n');
    }
    if let Some(p) = path.parent() {
        let _ = std::fs::create_dir_all(p);
    }
    let _ = std::fs::write(path, text);
}

fn append_line<T: Serialize>(path: &Path, item: &T) {
    use std::io::Write;
    if let Some(p) = path.parent() {
        let _ = std::fs::create_dir_all(p);
    }
    if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(path) {
        let _ = writeln!(f, "{}", serde_json::to_string(item).unwrap_or_default());
    }
}

/// Prompt history (`prompt-history.jsonl`).
#[derive(Clone, Debug, Default)]
pub struct History {
    pub path: Option<PathBuf>,
    pub entries: Vec<PromptInfo>,
    pub index: isize,
}

impl History {
    pub fn load(path: PathBuf) -> History {
        let mut entries: Vec<PromptInfo> = read_lines(&path).iter().filter_map(PromptInfo::parse).collect();
        if entries.len() > MAX_HISTORY {
            entries = entries.split_off(entries.len() - MAX_HISTORY);
        }
        if !entries.is_empty() {
            write_lines(&path, &entries);
        }
        History { path: Some(path), entries, index: 0 }
    }

    /// `move(direction, input)`; `None` when navigation is refused.
    pub fn step(&mut self, direction: isize, input: &str) -> Option<PromptInfo> {
        if self.entries.is_empty() {
            return None;
        }
        let len = self.entries.len() as isize;
        let at = |i: isize| -> Option<&PromptInfo> {
            let idx = if i >= 0 { i } else { len + i };
            if idx < 0 || idx >= len { None } else { self.entries.get(idx as usize) }
        };
        let current = at(self.index)?;
        if current.text != input && !input.is_empty() {
            return None;
        }
        let next = self.index + direction;
        if next.abs() > len || next > 0 {
            return None;
        }
        self.index = next;
        if next == 0 {
            return Some(PromptInfo::empty());
        }
        at(next).cloned()
    }

    pub fn append(&mut self, entry: PromptInfo) {
        if self.entries.last() == Some(&entry) {
            self.index = 0;
            return;
        }
        self.entries.push(entry.clone());
        self.index = 0;
        if self.entries.len() > MAX_HISTORY {
            self.entries = self.entries.split_off(self.entries.len() - MAX_HISTORY);
            if let Some(p) = &self.path {
                write_lines(p, &self.entries);
            }
            return;
        }
        if let Some(p) = &self.path {
            append_line(p, &entry);
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct StashEntry {
    pub prompt: PromptInfo,
    pub timestamp: i64,
}

/// Prompt stash (`prompt-stash.jsonl`).
#[derive(Clone, Debug, Default)]
pub struct Stash {
    pub path: Option<PathBuf>,
    pub entries: Vec<StashEntry>,
}

impl Stash {
    pub fn load(path: PathBuf) -> Stash {
        let mut entries: Vec<StashEntry> = read_lines(&path)
            .into_iter()
            .filter_map(|v| {
                let prompt = PromptInfo::parse(v.get("prompt")?)?;
                let timestamp = v.get("timestamp")?.as_f64()? as i64;
                Some(StashEntry { prompt, timestamp })
            })
            .collect();
        if entries.len() > MAX_STASH {
            entries = entries.split_off(entries.len() - MAX_STASH);
        }
        if !entries.is_empty() {
            write_lines(&path, &entries);
        }
        Stash { path: Some(path), entries }
    }
    pub fn push(&mut self, prompt: PromptInfo) {
        let e = StashEntry { prompt, timestamp: crate::util::locale::now_ms() };
        self.entries.push(e.clone());
        if self.entries.len() > MAX_STASH {
            self.entries = self.entries.split_off(self.entries.len() - MAX_STASH);
            self.save();
            return;
        }
        if let Some(p) = &self.path {
            append_line(p, &e);
        }
    }
    pub fn pop(&mut self) -> Option<PromptInfo> {
        let e = self.entries.pop()?;
        self.save();
        Some(e.prompt)
    }
    pub fn remove(&mut self, index: usize) -> Option<StashEntry> {
        if index >= self.entries.len() {
            return None;
        }
        let e = self.entries.remove(index);
        self.save();
        Some(e)
    }
    fn save(&self) {
        if let Some(p) = &self.path {
            write_lines(p, &self.entries);
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ModelKey {
    #[serde(rename = "providerID")]
    pub provider_id: String,
    #[serde(rename = "modelID")]
    pub model_id: String,
}

/// `model.json` (`src/model-preference.ts`): recent (≤10 MRU), favorite, variant; unknown keys kept.
#[derive(Clone, Debug, Default)]
pub struct ModelPrefs {
    pub path: Option<PathBuf>,
    pub recent: Vec<ModelKey>,
    pub favorite: Vec<ModelKey>,
    pub variant: Map<String, Value>,
}

fn models_of(v: Option<&Value>) -> Vec<ModelKey> {
    v.and_then(|v| v.as_array())
        .map(|a| {
            a.iter()
                .filter_map(|i| {
                    let p = i.get("providerID")?.as_str().filter(|s| !s.is_empty())?;
                    let m = i.get("modelID")?.as_str().filter(|s| !s.is_empty())?;
                    Some(ModelKey { provider_id: p.into(), model_id: m.into() })
                })
                .collect()
        })
        .unwrap_or_default()
}

impl ModelPrefs {
    pub fn load(path: PathBuf) -> ModelPrefs {
        let v: Value = std::fs::read_to_string(&path).ok().and_then(|t| serde_json::from_str(&t).ok()).unwrap_or(json!({}));
        let variant = v
            .get("variant")
            .and_then(|x| x.as_object())
            .map(|o| o.iter().filter(|(k, v)| !k.is_empty() && v.as_str().is_some_and(|s| !s.is_empty())).map(|(k, v)| (k.clone(), v.clone())).collect())
            .unwrap_or_default();
        ModelPrefs { recent: models_of(v.get("recent")), favorite: models_of(v.get("favorite")), variant, path: Some(path) }
    }

    fn update(&self, patch: Value) {
        let Some(path) = &self.path else { return };
        let mut current: Value = std::fs::read_to_string(path).ok().and_then(|t| serde_json::from_str(&t).ok()).unwrap_or(json!({}));
        if !current.is_object() {
            current = json!({});
        }
        for (k, v) in patch.as_object().unwrap() {
            current[k] = v.clone();
        }
        let _ = crate::util::paths::write_atomic(path, serde_json::to_string_pretty(&current).unwrap_or_default().as_bytes());
    }

    pub fn add_recent(&mut self, provider: &str, model: &str) {
        let key = ModelKey { provider_id: provider.into(), model_id: model.into() };
        let mut next = vec![key.clone()];
        for r in &self.recent {
            if next.len() >= 10 {
                break;
            }
            if !next.contains(r) {
                next.push(r.clone());
            }
        }
        self.recent = next;
        self.update(json!({ "recent": self.recent }));
    }

    pub fn set_favorite(&mut self, provider: &str, model: &str, enabled: bool) {
        let key = ModelKey { provider_id: provider.into(), model_id: model.into() };
        self.favorite.retain(|k| *k != key);
        if enabled {
            self.favorite.insert(0, key);
        }
        self.update(json!({ "favorite": self.favorite }));
    }

    pub fn is_favorite(&self, provider: &str, model: &str) -> bool {
        self.favorite.iter().any(|k| k.provider_id == provider && k.model_id == model)
    }

    pub fn set_variant(&mut self, key: &str, variant: Option<&str>) {
        match variant {
            Some(v) => {
                self.variant.insert(key.into(), Value::String(v.into()));
            }
            None => {
                self.variant.remove(key);
            }
        }
        self.update(json!({ "variant": self.variant }));
    }

    pub fn variant(&self, key: &str) -> Option<String> {
        self.variant.get(key).and_then(|v| v.as_str()).map(|s| s.to_string())
    }
}

/// `cycleModelVariant`.
pub fn cycle_variant(current: Option<&str>, variants: &[String]) -> Option<String> {
    let named: Vec<&String> = variants.iter().filter(|v| *v != "default").collect();
    if named.is_empty() {
        return None;
    }
    let value = current.filter(|c| *c != "default");
    match value {
        None => Some(named[0].clone()),
        Some(v) => match named.iter().position(|n| *n == v) {
            Some(i) if i + 1 < named.len() => Some(named[i + 1].clone()),
            _ => None,
        },
    }
}

/// `session.json` `{pinned: string[]}`.
#[derive(Clone, Debug, Default)]
pub struct SessionPins {
    pub path: Option<PathBuf>,
    pub pinned: Vec<String>,
}

impl SessionPins {
    pub fn load(path: PathBuf) -> SessionPins {
        let pinned = std::fs::read_to_string(&path)
            .ok()
            .and_then(|t| serde_json::from_str::<Value>(&t).ok())
            .and_then(|v| v.get("pinned").and_then(|p| p.as_array()).map(|a| a.iter().filter_map(|x| x.as_str().map(|s| s.to_string())).collect()))
            .unwrap_or_default();
        SessionPins { path: Some(path), pinned }
    }
    pub fn toggle(&mut self, id: &str) {
        if let Some(i) = self.pinned.iter().position(|p| p == id) {
            self.pinned.remove(i);
        } else {
            self.pinned.push(id.into());
        }
        if let Some(p) = &self.path {
            let _ = crate::util::paths::write_atomic(p, serde_json::to_string_pretty(&json!({ "pinned": self.pinned })).unwrap_or_default().as_bytes());
        }
    }
}

/// Keyed JSON store at `<state>/<channel>/tui/<key>.json`.
#[derive(Clone, Debug)]
pub struct KeyedStore {
    pub dir: PathBuf,
}

impl KeyedStore {
    pub fn new(state: &Path, channel: &str) -> KeyedStore {
        KeyedStore { dir: state.join(channel).join("tui") }
    }
    fn valid(key: &str) -> bool {
        regex::Regex::new(r"^[a-zA-Z0-9][a-zA-Z0-9._-]*$").unwrap().is_match(key)
    }
    pub fn get(&self, key: &str) -> Option<Value> {
        if !Self::valid(key) {
            return None;
        }
        std::fs::read_to_string(self.dir.join(format!("{key}.json"))).ok().and_then(|t| serde_json::from_str(&t).ok())
    }
    pub fn update(&self, key: &str, f: impl FnOnce(&mut Value)) {
        if !Self::valid(key) {
            return;
        }
        let mut v = self.get(key).unwrap_or(json!({}));
        f(&mut v);
        let _ = crate::util::paths::write_atomic(&self.dir.join(format!("{key}.json")), serde_json::to_string(&v).unwrap_or_default().as_bytes());
    }
}

/// `frecency.jsonl`: `{path, frequency, lastOpen}` lines, ≤1000.
pub fn frecency_touch(path: &Path, file: &str) {
    let mut entries = read_lines(path);
    let now = crate::util::locale::now_ms();
    let mut found = false;
    for e in entries.iter_mut() {
        if e.get("path").and_then(|p| p.as_str()) == Some(file) {
            let f = e.get("frequency").and_then(|f| f.as_f64()).unwrap_or(0.0) + 1.0;
            e["frequency"] = json!(f);
            e["lastOpen"] = json!(now);
            found = true;
        }
    }
    if !found {
        entries.push(json!({ "path": file, "frequency": 1, "lastOpen": now }));
    }
    if entries.len() > MAX_FRECENCY {
        entries = entries.split_off(entries.len() - MAX_FRECENCY);
    }
    write_lines(path, &entries);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("oc-tui-test-{}-{}", std::process::id(), crate::util::id_body(false)));
        std::fs::create_dir_all(&d).unwrap();
        d.join(name)
    }

    fn p(text: &str) -> PromptInfo {
        PromptInfo { text: text.into(), ..PromptInfo::empty() }
    }

    #[test]
    fn history_navigation() {
        let path = tmp("prompt-history.jsonl");
        let mut h = History::load(path.clone());
        h.append(p("one"));
        h.append(p("two"));
        h.append(p("two"));
        assert_eq!(h.entries.len(), 2);
        // At index 0 the current entry is the *oldest* ("one"): non-empty different input blocks.
        assert!(h.step(-1, "typed").is_none());
        assert_eq!(h.step(-1, "").unwrap().text, "two");
        assert_eq!(h.step(-1, "two").unwrap().text, "one");
        assert!(h.step(-1, "one").is_none());
        assert_eq!(h.step(1, "one").unwrap().text, "two");
        assert_eq!(h.step(1, "two").unwrap().text, "");
        let reloaded = History::load(path);
        assert_eq!(reloaded.entries.len(), 2);
    }

    #[test]
    fn stash_and_models() {
        let mut s = Stash::load(tmp("prompt-stash.jsonl"));
        s.push(p("a"));
        s.push(p("b"));
        assert_eq!(s.pop().unwrap().text, "b");
        assert_eq!(Stash::load(s.path.clone().unwrap()).entries.len(), 1);
        let path = tmp("model.json");
        std::fs::write(&path, r#"{"keep": 1}"#).unwrap();
        let mut m = ModelPrefs::load(path.clone());
        for i in 0..12 {
            m.add_recent("p", &format!("m{i}"));
        }
        m.add_recent("p", "m5");
        assert_eq!(m.recent.len(), 10);
        assert_eq!(m.recent[0].model_id, "m5");
        let raw: Value = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(raw["keep"], 1);
        assert_eq!(raw["recent"][0]["providerID"], "p");
        assert_eq!(cycle_variant(None, &["default".into(), "low".into(), "high".into()]), Some("low".into()));
        assert_eq!(cycle_variant(Some("high"), &["low".into(), "high".into()]), None);
    }
}
