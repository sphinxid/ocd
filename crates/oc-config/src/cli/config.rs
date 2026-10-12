//! `cli.json` load / merge / update (`packages/cli/src/config/config.ts`).

use std::path::{Path, PathBuf};

use serde_json::{Map, Value};

use super::migrate::{self, parse_jsonc, parse_record};
use super::schema::{SCHEMA_URL, decode, normalize_legacy_tabs};
use crate::jsonc_edit::{self, ModifyOptions, Segment};

/// `"Failed to update CLI config"` wrapping the cause.
#[derive(Debug, thiserror::Error)]
#[error("Failed to update CLI config")]
pub struct UpdateError {
    pub cause: String,
}

/// The CLI client configuration service.
#[derive(Debug, Clone)]
pub struct CliConfig {
    /// `<config>/cli.json`
    pub file: PathBuf,
    pub config_dir: PathBuf,
    pub state_dir: PathBuf,
    /// Decoded `OPENCODE_CLI_CONFIG_CONTENT` (read once at construction).
    pub content: Option<Value>,
}

/// Path of `cli.json` for a global config directory.
pub fn path(config_dir: &Path) -> PathBuf {
    config_dir.join("cli.json")
}

/// Decode inline content (`OPENCODE_CLI_CONFIG_CONTENT`); invalid → `None`.
pub fn decode_content(text: &str) -> Option<Value> {
    decode(&Value::Object(parse_record(text)?))
}

fn is_record(v: Option<&Value>) -> bool {
    v.is_some_and(Value::is_object)
}

/// Recursive deep merge of plain objects; arrays and scalars replace.
pub fn merge_records(base: &Map<String, Value>, overlay: &Map<String, Value>) -> Map<String, Value> {
    let mut result = base.clone();
    for (key, value) in overlay {
        let merged = if is_record(result.get(key)) && value.is_object() {
            Value::Object(merge_records(result[key].as_object().unwrap(), value.as_object().unwrap()))
        } else {
            value.clone()
        };
        result.insert(key.clone(), merged);
    }
    result
}

/// `merge(...values)`: legacy tabs normalized per value, deep merged, decoded (fallback `{}`).
pub fn merge(values: &[Option<&Value>]) -> Value {
    let mut result = Map::new();
    for value in values {
        let normalized = value.map(normalize_legacy_tabs).unwrap_or(Value::Object(Map::new()));
        if let Value::Object(o) = normalized {
            result = merge_records(&result, &o);
        }
    }
    decode(&Value::Object(result)).unwrap_or_else(|| Value::Object(Map::new()))
}

/// One leaf edit: `None` removes.
#[derive(Debug, Clone, PartialEq)]
pub struct Edit {
    pub path: Vec<String>,
    pub value: Option<Value>,
}

/// Diff `before` vs `after` into leaf edits (objects recurse, arrays/scalars replaced whole).
pub fn changes(before: Option<&Value>, after: Option<&Value>, path: &[String]) -> Vec<Edit> {
    if before == after {
        return vec![];
    }
    if let (Some(Value::Object(b)), Some(Value::Object(a))) = (before, after) {
        let mut keys: Vec<&String> = b.keys().collect();
        keys.extend(a.keys().filter(|k| !b.contains_key(*k)));
        let mut out = Vec::new();
        for key in keys {
            let mut p = path.to_vec();
            p.push(key.clone());
            if !a.contains_key(key) {
                out.push(Edit { path: p, value: None });
            } else if !b.contains_key(key) {
                out.push(Edit { path: p, value: a.get(key).cloned() });
            } else {
                out.extend(changes(b.get(key), a.get(key), &p));
            }
        }
        return out;
    }
    vec![Edit { path: path.to_vec(), value: after.cloned() }]
}

impl CliConfig {
    /// Construct from the global directories; reads `OPENCODE_CLI_CONFIG_CONTENT` once.
    pub fn new(config_dir: impl Into<PathBuf>, state_dir: impl Into<PathBuf>) -> CliConfig {
        let content = std::env::var("OPENCODE_CLI_CONFIG_CONTENT").ok().filter(|s| !s.is_empty());
        CliConfig::with_content(config_dir, state_dir, content.as_deref())
    }

    pub fn with_content(
        config_dir: impl Into<PathBuf>,
        state_dir: impl Into<PathBuf>,
        content: Option<&str>,
    ) -> CliConfig {
        let config_dir = config_dir.into();
        CliConfig {
            file: path(&config_dir),
            config_dir,
            state_dir: state_dir.into(),
            content: content.and_then(decode_content),
        }
    }

    fn read_file(&self) -> Option<Value> {
        let text = std::fs::read_to_string(&self.file).ok()?;
        decode(&Value::Object(parse_record(&text)?))
    }

    fn load(&self, migration: Option<Value>) -> Value {
        let file = migration.or_else(|| self.read_file());
        merge(&[file.as_ref(), self.content.as_ref()])
    }

    async fn lock(&self) -> Option<oc_util::flock::Lease> {
        let root = self.state_dir.join("locks");
        oc_util::flock::acquire(&root, &self.file.to_string_lossy(), oc_util::flock::Options::default()).await.ok()
    }

    fn migrate(&self) -> Result<Option<migrate::Migration>, String> {
        migrate::run(&self.file, &self.config_dir, &self.state_dir)
    }

    /// `get()`: under the file lock, migrate, then merge file + inline content.
    pub async fn get(&self) -> Value {
        let lease = self.lock().await;
        let migration = match self.migrate() {
            Ok(m) => m,
            Err(cause) => {
                tracing::warn!(cause = %cause, "failed to migrate cli config");
                None
            }
        };
        if let Some(cause) = migration.as_ref().and_then(|m| m.cause.as_ref()) {
            tracing::warn!(cause = %cause, "failed to persist migrated cli config");
        }
        let out = self.load(migration.map(|m| m.info));
        if let Some(lease) = lease {
            let _ = lease.release();
        }
        out
    }

    /// `update(fn)`: apply `f` to the effective config, write the leaf diff into the file text with
    /// comment-preserving JSONC edits, and return `merge(written, inlineContent)`.
    pub async fn update(&self, f: impl FnOnce(&mut Value)) -> Result<Value, UpdateError> {
        let lease = self.lock().await;
        let result = self.update_locked(f);
        if let Some(lease) = lease {
            let _ = lease.release();
        }
        result.map_err(|cause| UpdateError { cause })
    }

    fn update_locked(&self, f: impl FnOnce(&mut Value)) -> Result<Value, String> {
        let migration = self.migrate()?;
        if let Some(cause) = migration.as_ref().and_then(|m| m.cause.clone()) {
            return Err(cause);
        }
        let current = self.load(migration.map(|m| m.info));
        let mut next = current.clone();
        f(&mut next);
        let edits = changes(Some(&current), Some(&next), &[]);
        if edits.is_empty() {
            return Ok(current);
        }
        let mut text = std::fs::read_to_string(&self.file).unwrap_or_else(|_| {
            serde_json::to_string_pretty(&serde_json::json!({ "$schema": SCHEMA_URL })).unwrap_or_default()
        });
        for edit in &edits {
            let segments: Vec<Segment> = edit.path.iter().map(|s| Segment::from(s.as_str())).collect();
            text = jsonc_edit::apply_modify(&text, &segments, edit.value.as_ref(), &ModifyOptions::formatted())?;
        }
        let config = parse_jsonc(&text).and_then(|v| decode(&v)).ok_or_else(|| "Invalid CLI config update".to_string())?;
        let text = if text.ends_with('\n') { text } else { format!("{text}\n") };
        let temp = PathBuf::from(format!("{}.tmp", self.file.display()));
        if let Some(parent) = self.file.parent() {
            std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
        }
        migrate::write_private(&temp, &text).map_err(|e| e.to_string())?;
        std::fs::rename(&temp, &self.file).map_err(|e| e.to_string())?;
        Ok(merge(&[Some(&config), self.content.as_ref()]))
    }
}
