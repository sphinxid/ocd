//! Loading the ordered entry list (`core/src/config.ts` `load`, `parseInfo`, `loadFile`).

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use serde_json::Value;

use crate::decode::Decode;
use crate::discovery::{self, NAMES, Options, Sources};
use crate::normalize::{NormalizeResult, normalize};
use crate::schema::{Entry, Info};

/// Fatal location load errors (a defect upstream; the previous config stays active on reload).
#[derive(Debug, thiserror::Error)]
pub enum LoadError {
    /// `{file:}` substitution failure (`ConfigInvalidError`).
    #[error("{0}")]
    Invalid(#[from] oc_util::variable::ConfigInvalidError),
    /// Read error other than NotFound / PermissionDenied.
    #[error("failed to read {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
}

impl LoadError {
    /// `{ name: "ConfigInvalidError", data: { path, message } }` when applicable.
    pub fn to_named_error(&self) -> Value {
        match self {
            LoadError::Invalid(e) => serde_json::json!({
                "name": "ConfigInvalidError",
                "data": { "path": e.path, "message": e.message },
            }),
            LoadError::Io { path, source } => serde_json::json!({
                "name": "UnknownError",
                "data": { "message": format!("failed to read {}: {}", path.display(), source) },
            }),
        }
    }
}

/// TUI rendering of a `ConfigInvalidError`
/// (`Configuration is invalid at <path>: <message>` + `↳ <issue> <path>` lines).
pub fn format_invalid(path: &str, message: Option<&str>, issues: &[(String, Vec<String>)]) -> String {
    let mut out = String::from("Configuration is invalid");
    if !path.is_empty() && path != "config" {
        out.push_str(&format!(" at {path}"));
    }
    if let Some(m) = message {
        out.push_str(&format!(": {m}"));
    }
    for (message, path) in issues {
        out.push_str(&format!("\n↳ {} {}", message, path.join(".")));
    }
    out
}

/// A registered wellknown (remote organization) source, as provided by the core.
#[derive(Debug, Clone, PartialEq)]
pub struct WellKnownEntry {
    pub origin: String,
    pub integration_id: String,
}

/// Implemented by the core: wellknown manifest resolution, credentials and caching.
#[async_trait::async_trait]
pub trait WellKnown: Send + Sync {
    /// Registered sources (`wellknown.entries()`).
    async fn entries(&self) -> anyhow::Result<Vec<WellKnownEntry>>;
    /// `{ [manifest.auth.env]: key }` when the latest credential is a `key` credential.
    async fn variables(&self, entry: &WellKnownEntry) -> Option<HashMap<String, String>>;
    /// Resolve (and cache) the configs for a source.
    async fn resolve(&self, entry: &WellKnownEntry, variables: &HashMap<String, String>) -> anyhow::Result<Vec<Value>>;
    /// Last successfully resolved configs for an origin.
    async fn cached(&self, origin: &str) -> Vec<Value>;
}

fn warn(source: &str, path: &str, kind: &str, action: &str) {
    tracing::warn!(source = %source, path = %path, kind = %kind, action = %action, "configuration normalization diagnostic");
}

/// `parseInfo(text, source)`: JSONC parse → normalize (logging diagnostics) → final decode.
pub fn parse_info(text: &str, source: &str) -> Option<Info> {
    let Ok(input) = oc_util::jsonc::parse(text) else {
        warn(source, "$", "invalid", "rejected malformed JSON or JSONC document");
        return None;
    };
    let result = normalize(&input);
    for d in result.diagnostics() {
        warn(source, &d.log_path(), d.kind.as_str(), d.message);
    }
    let NormalizeResult::Normalized { encoded, .. } = result else { return None };
    let info = Info::decode(&encoded);
    if info.is_none() {
        warn(source, "$", "invalid", "rejected canonical configuration after final validation");
    }
    info
}

/// Read a file, treating NotFound / PermissionDenied as absent.
pub fn read_safe(path: &Path) -> Result<Option<String>, LoadError> {
    match std::fs::read(path) {
        Ok(bytes) => Ok(Some(String::from_utf8_lossy(&bytes).into_owned())),
        Err(e) if matches!(e.kind(), std::io::ErrorKind::NotFound | std::io::ErrorKind::PermissionDenied) => Ok(None),
        // A directory or other non-file at a candidate path behaves like an unreadable file.
        Err(e) if e.kind() == std::io::ErrorKind::IsADirectory => Ok(None),
        Err(e) => Err(LoadError::Io { path: path.to_path_buf(), source: e }),
    }
}

/// `loadFile`: read → `{env:}`/`{file:}` substitution → parseInfo → Document.
pub fn load_file(path: &Path) -> Result<Option<Entry>, LoadError> {
    let Some(text) = read_safe(path)? else { return Ok(None) };
    let source = path.to_string_lossy().to_string();
    let dir = path.parent().unwrap_or(Path::new("/"));
    let substituted =
        oc_util::variable::substitute(oc_util::variable::Input { text: &text, dir, source: &source, env: None })?;
    Ok(parse_info(&substituted, &source).map(|info| Entry::Document { path: Some(path.to_path_buf()), info }))
}

/// `loadDirectory`: `opencode.json`, `opencode.jsonc`, then `Directory`.
pub fn load_directory(directory: &Path) -> Result<Vec<Entry>, LoadError> {
    let mut out = Vec::new();
    for name in NAMES {
        if let Some(entry) = load_file(&directory.join(name))? {
            out.push(entry);
        }
    }
    out.push(Entry::Directory { path: directory.to_path_buf() });
    Ok(out)
}

/// Inline content (`OPENCODE_CONFIG_CONTENT`) document; `{file:}` base dir = location directory.
pub fn load_content(content: &str, directory: &Path) -> Result<Option<Entry>, LoadError> {
    let source = "OPENCODE_CONFIG_CONTENT";
    let text =
        oc_util::variable::substitute(oc_util::variable::Input { text: content, dir: directory, source, env: None })?;
    Ok(parse_info(&text, source).map(|info| Entry::Document { path: None, info }))
}

/// Location config loader.
#[derive(Clone)]
pub struct Loader {
    /// Global config directory `G`.
    pub global_config: PathBuf,
    /// `Global.home`.
    pub home: PathBuf,
    /// Location working directory `D`.
    pub directory: PathBuf,
    pub options: Options,
    pub wellknown: Option<Arc<dyn WellKnown>>,
}

impl Loader {
    pub fn new(global: &oc_util::global::Global, directory: impl Into<PathBuf>, options: Options) -> Loader {
        Loader {
            global_config: global.config.clone(),
            home: global.home.clone(),
            directory: directory.into(),
            options,
            wellknown: None,
        }
    }

    pub fn with_wellknown(mut self, wellknown: Arc<dyn WellKnown>) -> Loader {
        self.wellknown = Some(wellknown);
        self
    }

    pub fn discover(&self) -> Sources {
        discovery::discover(&self.global_config, &self.home, &self.directory, &self.options)
    }

    /// Wellknown documents (lowest priority).
    pub async fn load_wellknown(&self) -> Result<Vec<Entry>, LoadError> {
        let Some(wellknown) = &self.wellknown else { return Ok(vec![]) };
        let entries = match wellknown.entries().await {
            Ok(entries) => entries,
            Err(error) => {
                tracing::warn!(error = %error, "failed to discover wellknown config");
                return Ok(vec![]);
            }
        };
        let mut out = Vec::new();
        for entry in entries {
            let variables = wellknown.variables(&entry).await;
            let configs = match &variables {
                Some(vars) => match wellknown.resolve(&entry, vars).await {
                    Ok(configs) => configs,
                    Err(error) => {
                        tracing::warn!(source = %entry.origin, error = %error, "failed to load wellknown config");
                        wellknown.cached(&entry.origin).await
                    }
                },
                None => {
                    tracing::warn!(source = %entry.origin, error = "No usable wellknown credential", "failed to load wellknown config");
                    wellknown.cached(&entry.origin).await
                }
            };
            for config in configs {
                let text = serde_json::to_string(&config).unwrap_or_default();
                let substituted = oc_util::variable::substitute(oc_util::variable::Input {
                    text: &text,
                    dir: Path::new(&entry.origin),
                    source: &entry.origin,
                    env: variables.as_ref(),
                })?;
                if let Some(info) = parse_info(&substituted, &entry.origin) {
                    out.push(Entry::Document { path: None, info });
                }
            }
        }
        Ok(out)
    }

    /// `Config.load(sources)`: wellknown, global dir, explicit, direct, `.opencode` dirs, content.
    pub async fn load(&self, sources: &Sources) -> Result<Vec<Entry>, LoadError> {
        let wellknown = self.load_wellknown().await?;
        Ok([wellknown, self.load_local(sources)?].concat())
    }

    /// Everything except wellknown documents, in priority order.
    pub fn load_local(&self, sources: &Sources) -> Result<Vec<Entry>, LoadError> {
        let mut direct = Vec::new();
        for file in &sources.direct {
            if let Some(entry) = load_file(file)? {
                direct.push(entry);
            }
        }
        let explicit = match &sources.explicit {
            Some(file) => load_file(file)?.into_iter().collect(),
            None => vec![],
        };
        let content = match &self.options.content {
            Some(content) => load_content(content, &self.directory)?.into_iter().collect(),
            None => vec![],
        };
        let global = match &sources.global {
            Some(dir) => load_directory(dir)?,
            None => vec![],
        };
        let mut project = Vec::new();
        for root in sources.project.iter().filter(|r| r.present) {
            project.extend(load_directory(&root.path)?);
        }
        Ok([global, explicit, direct, project, content].concat())
    }

    /// Discover and load in one step.
    pub async fn entries(&self) -> Result<(Sources, Vec<Entry>), LoadError> {
        let sources = self.discover();
        let entries = self.load(&sources).await?;
        Ok((sources, entries))
    }
}

/// Reload decision: publish `config.updated` only when entries or compatibility roots changed.
pub fn changed(previous: (&Sources, &[Entry]), next: (&Sources, &[Entry])) -> bool {
    previous.1 != next.1 || previous.0.claude != next.0.claude || previous.0.agents != next.0.agents
}

/// `PATCH /api/experimental/config` (`Config.update`): set or remove `shell` in the last existing
/// of `<dir>/opencode.json`, `<dir>/opencode.jsonc` (else `opencode.jsonc`), preserving comments.
/// `directory` is the initial global dir (or `Global.config`). Returns the written path.
pub fn update_shell(directory: &Path, shell: Option<&str>) -> std::io::Result<PathBuf> {
    let file = NAMES
        .iter()
        .map(|n| directory.join(n))
        .filter(|p| p.is_file())
        .last()
        .unwrap_or_else(|| directory.join("opencode.jsonc"));
    let text = match std::fs::read_to_string(&file) {
        Ok(t) => t,
        Err(e) if matches!(e.kind(), std::io::ErrorKind::NotFound | std::io::ErrorKind::PermissionDenied) => {
            "{}\n".to_string()
        }
        Err(e) => return Err(e),
    };
    let value = shell.map(|s| Value::String(s.to_string()));
    let edits = crate::jsonc_edit::modify(
        &text,
        &[crate::jsonc_edit::Segment::from("shell")],
        value.as_ref(),
        &crate::jsonc_edit::ModifyOptions::formatted(),
    )
    .map_err(std::io::Error::other)?;
    let updated = crate::jsonc_edit::apply_edits(&text, &edits).map_err(std::io::Error::other)?;
    let updated = if updated.ends_with('\n') { updated } else { format!("{updated}\n") };
    if let Some(parent) = file.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(&file, updated)?;
    Ok(file)
}
