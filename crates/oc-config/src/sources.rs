//! Directory-based configuration produced from `Directory` entries: Markdown agents
//! (`config/plugin/agent.ts`), Markdown commands (`config/plugin/command.ts`), skills
//! (`config/plugin/skill.ts`, `skill-file.ts`, `compatibility.ts`) and plugin directories
//! (`plugin/source-directory.ts`, `config/plugin/source.ts`).

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use indexmap::IndexMap;
use serde::Serialize;
use serde_json::{Map, Value};

use crate::decode::Decode;
use crate::discovery::contains;
use crate::legacy;
use crate::markdown;
use crate::schema::{AGENT_FIELDS, Agent, Command, Entry, Info, Plugin};

// ---------------------------------------------------------------------------------------------
// File scanning
// ---------------------------------------------------------------------------------------------

/// Files (following symlinks) ending in `suffix` directly in `dir` or, when `recursive`, below it.
fn scan(dir: &Path, recursive: bool, accept: &dyn Fn(&Path) -> bool, out: &mut Vec<PathBuf>, seen: &mut HashSet<PathBuf>) {
    let Ok(canonical) = std::fs::canonicalize(dir) else { return };
    if !seen.insert(canonical) {
        return;
    }
    let Ok(read) = std::fs::read_dir(dir) else { return };
    for entry in read.flatten() {
        let path = entry.path();
        let Ok(meta) = std::fs::metadata(&path) else { continue };
        if meta.is_dir() {
            if recursive {
                scan(&path, true, accept, out, seen);
            }
        } else if meta.is_file() && accept(&path) {
            out.push(path);
        }
    }
}

fn md(path: &Path) -> bool {
    path.extension().is_some_and(|e| e == "md")
}

/// `{<names>}/**/*.md` (or `/*.md` when not recursive) under `root`, sorted.
pub fn scan_markdown(root: &Path, names: &[&str], recursive: bool) -> Vec<PathBuf> {
    let mut out = Vec::new();
    for name in names {
        scan(&root.join(name), recursive, &md, &mut out, &mut HashSet::new());
    }
    out.sort();
    out.dedup();
    out
}

fn relative_name(directory: &Path, file: &Path, prefixes: &[&str]) -> String {
    let rel = file.strip_prefix(directory).unwrap_or(file).to_string_lossy().replace('\\', "/");
    let mut name = rel.as_str();
    for p in prefixes {
        if let Some(rest) = name.strip_prefix(&format!("{p}/")) {
            name = rest;
            break;
        }
    }
    name.strip_suffix(".md").unwrap_or(name).to_string()
}

// ---------------------------------------------------------------------------------------------
// Agents
// ---------------------------------------------------------------------------------------------

/// Directories (relative to a config root) holding Markdown agents.
pub const AGENT_SOURCE_DIRECTORIES: [&str; 4] = ["agent", "agents", "mode", "modes"];

/// Whether a changed path is an agent source (`isAgentSource`).
pub fn is_agent_source(entries: &[Entry], file: &Path) -> bool {
    is_source(entries, file, &AGENT_SOURCE_DIRECTORIES)
}

/// Whether a changed path is a command source (`isCommandSource`).
pub fn is_command_source(entries: &[Entry], file: &Path) -> bool {
    is_source(entries, file, &COMMAND_SOURCE_DIRECTORIES)
}

/// Whether a changed path is under a plugin directory (`isPluginSource`).
pub fn is_plugin_source(entries: &[Entry], file: &Path) -> bool {
    is_source(entries, file, &PLUGIN_DIRECTORIES)
}

fn is_source(entries: &[Entry], file: &Path, names: &[&str]) -> bool {
    entries
        .iter()
        .filter_map(Entry::directory)
        .any(|dir| names.iter().any(|name| contains(&dir.join(name), file)))
}

/// Agent fields only V2 understands (`nativeKeys` in `config/plugin/agent.ts`).
const NATIVE_AGENT_KEYS: [&str; 4] = ["permissions", "request", "system", "disabled"];

/// Decode one Markdown agent file into a `Document` (`decode` in `config/plugin/agent.ts`).
pub fn decode_agent_file(directory: &Path, file: &Path, primary: bool, content: &str) -> Option<Entry> {
    let markdown = markdown::parse_option(content)?;
    let name = relative_name(directory, file, &AGENT_SOURCE_DIRECTORIES);
    let body = crate::schema::js_trim(&markdown.content).to_string();
    let data = markdown.data;
    let unsupported: Vec<&String> =
        data.keys().filter(|k| k.as_str() != "variant" && !AGENT_FIELDS.contains(&k.as_str())).collect();
    // Fields only V2 understands. A file that uses any of them is read as V2, whatever else it contains.
    let is_legacy = !unsupported.is_empty() && !data.keys().any(|k| NATIVE_AGENT_KEYS.contains(&k.as_str()));
    if !is_legacy && !unsupported.is_empty() {
        tracing::warn!(path = %file.display(), fields = ?unsupported, "ignored unsupported agent fields");
    }
    let agent: Value = if is_legacy {
        let mut input = Map::new();
        input.insert("name".into(), Value::String(name.clone()));
        for (k, v) in &data {
            input.insert(k.clone(), v.clone());
        }
        input.insert("prompt".into(), Value::String(body));
        let decoded = legacy::decode_agent(&Value::Object(input))?;
        legacy::migrate_agent(&decoded)
    } else {
        let mut input = data.clone();
        if let (Some(Value::String(model)), Some(Value::String(variant))) = (data.get("model"), data.get("variant"))
            && !model.contains('#')
            && !variant.is_empty()
            && !variant.contains('#')
        {
            input.insert("model".into(), Value::String(format!("{model}#{variant}")));
        }
        input.insert("system".into(), Value::String(body));
        crate::decode::encode(&Agent::decode(&Value::Object(input))?)
    };
    let mut agent = agent;
    if primary && let Value::Object(a) = &mut agent {
        a.insert("mode".into(), Value::String("primary".into()));
    }
    let info = Info::decode(&serde_json::json!({ "agents": { name: agent } }))?;
    Some(Entry::Document { path: Some(file.to_path_buf()), info })
}

/// Markdown agent documents for one config root (`{agent,agents}/**/*.md`, then `{mode,modes}/*.md`).
pub fn markdown_agents(directory: &Path) -> Vec<Entry> {
    let mut out = Vec::new();
    for (names, recursive, primary) in [(&["agent", "agents"], true, false), (&["mode", "modes"], false, true)] {
        for file in scan_markdown(directory, names, recursive) {
            let Ok(Some(content)) = crate::load::read_safe(&file) else { continue };
            if content.is_empty() {
                continue;
            }
            if let Some(entry) = decode_agent_file(directory, &file, primary, &content) {
                out.push(entry);
            }
        }
    }
    out
}

/// The agent plugin's document list: documents unchanged, each `Directory` replaced by its
/// Markdown agent documents.
pub fn agent_documents(entries: &[Entry]) -> Vec<Entry> {
    entries
        .iter()
        .flat_map(|entry| match entry {
            Entry::Document { .. } => vec![entry.clone()],
            Entry::Directory { path } => markdown_agents(path),
        })
        .collect()
}

// ---------------------------------------------------------------------------------------------
// Commands
// ---------------------------------------------------------------------------------------------

pub const COMMAND_SOURCE_DIRECTORIES: [&str; 2] = ["command", "commands"];

/// Decode one Markdown command file (`template = content.trim()`).
pub fn decode_command_file(directory: &Path, file: &Path, content: &str) -> Option<(String, Command)> {
    let markdown = markdown::parse_option(content)?;
    let mut input = markdown.data;
    input.insert("template".into(), Value::String(crate::schema::js_trim(&markdown.content).to_string()));
    let command = Command::decode(&Value::Object(input))?;
    Some((relative_name(directory, file, &COMMAND_SOURCE_DIRECTORIES), command))
}

/// Markdown commands of one config root, sorted by path.
pub fn markdown_commands(directory: &Path) -> Vec<(String, Command)> {
    scan_markdown(directory, &COMMAND_SOURCE_DIRECTORIES, true)
        .into_iter()
        .filter_map(|file| {
            let content = crate::load::read_safe(&file).ok()??;
            decode_command_file(directory, &file, &content)
        })
        .collect()
}

/// Effective commands: per name, later definitions replace earlier ones entirely; Markdown
/// commands occupy their Directory's slot.
pub fn commands(entries: &[Entry]) -> IndexMap<String, Command> {
    let mut out = IndexMap::new();
    for entry in entries {
        match entry {
            Entry::Document { info, .. } => {
                for (name, command) in info.commands.iter().flatten() {
                    out.insert(name.clone(), command.clone());
                }
            }
            Entry::Directory { path } => {
                for (name, command) in markdown_commands(path) {
                    out.insert(name, command);
                }
            }
        }
    }
    out
}

/// Prefix text for commands run as subagents.
pub const SUBAGENT_PREFIX: &str = "You are a subagent spawned by another session.";

// ---------------------------------------------------------------------------------------------
// Skills
// ---------------------------------------------------------------------------------------------

/// Parsed skill file (`Skill.Info`).
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Skill {
    pub id: String,
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub autoinvoke: Option<bool>,
    pub path: PathBuf,
    pub content: String,
}

/// Why a skill file was skipped.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SkillSkip {
    Markdown,
    Frontmatter,
}

fn boolean_value(value: Option<&Value>) -> Option<bool> {
    match value? {
        Value::Bool(b) => Some(*b),
        Value::Number(n) => match n.as_f64() {
            Some(1.0) => Some(true),
            Some(0.0) => Some(false),
            _ => None,
        },
        Value::String(s) => match s.trim().to_lowercase().as_str() {
            "true" | "yes" | "on" | "1" => Some(true),
            "false" | "no" | "off" | "0" => Some(false),
            _ => None,
        },
        _ => None,
    }
}

/// `SkillFile.parse(directory, filepath, content)`.
pub fn parse_skill(directory: &Path, file: &Path, content: &str) -> Result<Skill, SkillSkip> {
    let markdown = markdown::parse_option(content).ok_or(SkillSkip::Markdown)?;
    let fm = &markdown.data;
    let string_field = |key: &str| -> Result<Option<String>, SkillSkip> {
        match fm.get(key) {
            None => Ok(None),
            Some(Value::String(s)) => Ok(Some(s.clone())),
            Some(_) => Err(SkillSkip::Frontmatter),
        }
    };
    let name = string_field("name")?;
    let description = string_field("description")?;
    let basename = file.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
    let parent = file.parent().unwrap_or(Path::new("/"));
    let id = if parent == directory && basename != "SKILL.md" {
        basename.strip_suffix(".md").unwrap_or(&basename).to_string()
    } else {
        parent.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default()
    };
    let metadata = fm.get("metadata").and_then(Value::as_object);
    let autoinvoke = metadata
        .and_then(|m| boolean_value(m.get("opencode/autoinvoke")))
        .or_else(|| (boolean_value(fm.get("disable-model-invocation")) == Some(true)).then_some(false));
    Ok(Skill {
        name: name.unwrap_or_else(|| id.clone()),
        id,
        description,
        autoinvoke,
        path: file.to_path_buf(),
        content: markdown.content,
    })
}

/// Maximum directory depth of the skill scan (`SkillScan`).
const SKILL_SCAN_MAX_DEPTH: usize = 6;

/// `SkillScan.scan`: a breadth-first scan to depth 6 that stops descending at a directory holding a
/// `SKILL.md` (that file is the directory's skill). Top-level `*.md` files are skills too. Dot
/// directories and files, `node_modules`, `.git`, `.venv`, `venv` and `__pycache__` are skipped.
/// Symlinks are followed. Sorted.
pub fn scan_skill_files(directory: &Path) -> Vec<PathBuf> {
    #[derive(PartialEq)]
    enum Kind {
        Directory,
        File,
        Other,
    }
    let mut files = Vec::new();
    let mut queue: std::collections::VecDeque<(PathBuf, usize)> = std::collections::VecDeque::from([(directory.to_path_buf(), 0)]);
    while let Some((dir, depth)) = queue.pop_front() {
        let Ok(read) = std::fs::read_dir(&dir) else { continue };
        let children: Vec<(String, PathBuf, Kind)> = read
            .flatten()
            .filter_map(|entry| {
                let name = entry.file_name().to_string_lossy().to_string();
                if name.starts_with('.') || crate::discovery::IGNORED_DIRECTORIES.contains(&name.as_str()) {
                    return None;
                }
                let path = entry.path();
                let kind = match entry.file_type() {
                    Ok(t) if t.is_symlink() => match std::fs::metadata(&path) {
                        Ok(m) if m.is_dir() => Kind::Directory,
                        Ok(m) if m.is_file() => Kind::File,
                        _ => Kind::Other,
                    },
                    Ok(t) if t.is_dir() => Kind::Directory,
                    Ok(t) if t.is_file() => Kind::File,
                    _ => Kind::Other,
                };
                Some((name, path, kind))
            })
            .collect();
        if let Some((_, skill, _)) = children.iter().find(|(name, _, kind)| *kind == Kind::File && name == "SKILL.md") {
            files.push(skill.clone());
            continue;
        }
        if depth == 0 {
            files.extend(children.iter().filter(|(name, _, kind)| *kind == Kind::File && name.ends_with(".md")).map(|(_, p, _)| p.clone()));
        }
        if depth < SKILL_SCAN_MAX_DEPTH {
            queue.extend(children.into_iter().filter(|(_, _, kind)| *kind == Kind::Directory).map(|(_, p, _)| (p, depth + 1)));
        }
    }
    files.sort();
    files
}

/// Parse every skill in a directory (later ids replace earlier ones, keeping first position).
pub fn scan_skills(directory: &Path) -> Vec<Skill> {
    let mut out: IndexMap<String, Skill> = IndexMap::new();
    for file in scan_skill_files(directory) {
        let Ok(Some(content)) = crate::load::read_safe(&file) else { continue };
        if content.is_empty() {
            continue;
        }
        match parse_skill(directory, &file, &content) {
            Ok(skill) => {
                out.insert(skill.id.clone(), skill);
            }
            Err(reason) => tracing::debug!(filepath = %file.display(), reason = ?reason, "skill file skipped"),
        }
    }
    out.into_values().collect()
}

/// A skill source (`Skill.DirectorySource | Skill.UrlSource`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum SkillSource {
    Directory { path: PathBuf },
    Url { url: String },
}

/// Skill sources: `<dir>/skill`, `<dir>/skills` per Directory, then configured items
/// (`http(s)` URL, `~/` → home, absolute, else joined to the location directory); deduplicated.
pub fn skill_sources(entries: &[Entry], home: &Path, location: &Path) -> Vec<SkillSource> {
    let mut out: Vec<SkillSource> = Vec::new();
    let mut add = |s: SkillSource| {
        if !out.contains(&s) {
            out.push(s);
        }
    };
    for dir in entries.iter().filter_map(Entry::directory) {
        add(SkillSource::Directory { path: dir.join("skill") });
        add(SkillSource::Directory { path: dir.join("skills") });
    }
    for item in entries.iter().filter_map(Entry::info).flat_map(|i| i.skills.iter().flatten()) {
        if let Ok(url) = url::Url::parse(item)
            && (url.scheme() == "http" || url.scheme() == "https")
        {
            add(SkillSource::Url { url: item.clone() });
            continue;
        }
        let expanded = match item.strip_prefix("~/") {
            Some(rest) => home.join(rest),
            None => PathBuf::from(item),
        };
        let path = if expanded.is_absolute() { expanded } else { crate::discovery::lexical(&location.join(expanded)) };
        add(SkillSource::Directory { path });
    }
    out
}

/// Skills from compatibility roots (`<root>/skills`, realpath-resolved; later ids win).
pub fn compatibility_skills(claude: &[PathBuf], agents: &[PathBuf]) -> Vec<Skill> {
    let mut out: IndexMap<String, Skill> = IndexMap::new();
    for root in claude.iter().chain(agents) {
        let Ok(resolved) = std::fs::canonicalize(root.join("skills")) else { continue };
        for skill in scan_skills(&resolved) {
            out.insert(skill.id.clone(), skill);
        }
    }
    out.into_values().collect()
}

// ---------------------------------------------------------------------------------------------
// Plugins
// ---------------------------------------------------------------------------------------------

pub const PLUGIN_DIRECTORIES: [&str; 2] = ["plugin", "plugins"];

/// `PluginSourceDirectory.discover`: children of `plugin/` and `plugins/` (sorted by path) that
/// are `.ts`/`.js` files, directories, or symlinks to either.
pub fn discover_plugin_directory(directory: &Path) -> Vec<PathBuf> {
    let mut children: Vec<(PathBuf, std::fs::FileType)> = Vec::new();
    for name in PLUGIN_DIRECTORIES {
        let Ok(read) = std::fs::read_dir(directory.join(name)) else { continue };
        for entry in read.flatten() {
            if let Ok(ft) = entry.file_type() {
                children.push((entry.path(), ft));
            }
        }
    }
    children.sort_by(|a, b| a.0.cmp(&b.0));
    children
        .into_iter()
        .filter(|(target, ft)| {
            let s = target.to_string_lossy();
            let source = s.ends_with(".ts") || s.ends_with(".js");
            if ft.is_file() {
                return source;
            }
            if ft.is_dir() {
                return true;
            }
            if !ft.is_symlink() {
                return false;
            }
            let meta = std::fs::metadata(target);
            (source && meta.as_ref().is_ok_and(|m| m.is_file())) || meta.is_ok_and(|m| m.is_dir())
        })
        .map(|(t, _)| t)
        .collect()
}

/// A plugin enablement operation.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum PluginOperation {
    Add { target: String, options: Map<String, Value> },
    Remove { target: String },
}

/// Parse one configured plugin item.
pub fn parse_plugin(input: &Plugin) -> Result<PluginOperation, String> {
    match input {
        Plugin::Entry(e) => Ok(PluginOperation::Add {
            target: e.package.clone(),
            options: e.options.clone().map(|o| o.into_iter().collect()).unwrap_or_default(),
        }),
        Plugin::Spec(s) if !s.starts_with('-') => Ok(PluginOperation::Add { target: s.clone(), options: Map::new() }),
        Plugin::Spec(s) if s.len() == 1 => Err("Plugin remove operation requires a target".into()),
        Plugin::Spec(s) => Ok(PluginOperation::Remove { target: s[1..].to_string() }),
    }
}

/// Plugin operations: auto-discovered directory plugins first, then configured items from all
/// documents in order (`file://` → path, `./`/`../` → resolved against the declaring document's
/// directory or the location directory). Absolute targets that are files are skipped with
/// `"configured plugin path must be a directory"`. Entrypoint/mtime resolution is left to core.
pub fn plugin_operations(entries: &[Entry], location: &Path) -> Result<Vec<PluginOperation>, String> {
    let mut out: Vec<PluginOperation> = entries
        .iter()
        .filter_map(Entry::directory)
        .flat_map(|d| discover_plugin_directory(d))
        .map(|t| PluginOperation::Add { target: t.to_string_lossy().to_string(), options: Map::new() })
        .collect();
    for entry in entries {
        let Entry::Document { path, info } = entry else { continue };
        let directory = path.as_ref().and_then(|p| p.parent()).map(Path::to_path_buf).unwrap_or(location.to_path_buf());
        for item in info.plugins.iter().flatten() {
            let operation = match parse_plugin(item)? {
                PluginOperation::Add { target, options } => {
                    let target = if let Some(url) = target.strip_prefix("file://").map(|_| &target) {
                        url::Url::parse(url)
                            .ok()
                            .and_then(|u| u.to_file_path().ok())
                            .map(|p| p.to_string_lossy().to_string())
                            .unwrap_or(target.clone())
                    } else if target.starts_with("./") || target.starts_with("../") {
                        crate::discovery::lexical(&directory.join(&target)).to_string_lossy().to_string()
                    } else {
                        target
                    };
                    PluginOperation::Add { target, options }
                }
                remove => remove,
            };
            if let PluginOperation::Add { target, .. } = &operation
                && Path::new(target).is_absolute()
                && Path::new(target).is_file()
            {
                tracing::warn!(target = %target, "configured plugin path must be a directory");
                continue;
            }
            out.push(operation);
        }
    }
    Ok(out)
}
