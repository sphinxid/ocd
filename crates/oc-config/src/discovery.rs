//! Config source discovery (`core/src/config/discovery.ts`) and the watch plan
//! (`core/src/config/watch.ts`).

use std::path::{Component, Path, PathBuf};

use serde::Serialize;

/// Config file names, lowest priority first within one directory.
pub const NAMES: [&str; 2] = ["opencode.json", "opencode.jsonc"];

/// `Config.Options`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Options {
    /// `Some(false)` skips the upward project walk entirely.
    pub project: Option<bool>,
    /// `Some(false)` skips the global config dir, `~/.claude` and `~/.agents`.
    pub global: Option<bool>,
    /// Explicit config file (`OPENCODE_CONFIG`), resolved against the process cwd.
    pub file: Option<String>,
    /// Inline JSONC (`OPENCODE_CONFIG_CONTENT`).
    pub content: Option<String>,
}

fn truthy(v: Option<String>) -> bool {
    v.is_some_and(|v| v == "1" || v.to_lowercase() == "true")
}

impl Options {
    /// Server-process wiring from the environment (`server-process.ts:115-122`).
    pub fn from_env() -> Options {
        let project_disable = std::env::var("OPENCODE_CONFIG_PROJECT_DISABLE")
            .ok()
            .or_else(|| std::env::var("OPENCODE_DISABLE_PROJECT_CONFIG").ok());
        Options {
            project: Some(!truthy(project_disable)),
            global: None,
            file: std::env::var("OPENCODE_CONFIG").ok(),
            content: std::env::var("OPENCODE_CONFIG_CONTENT").ok(),
        }
    }
}

/// `OPENCODE_FILEWATCHER_DISABLE` / `OPENCODE_DISABLE_FILEWATCHER` truthy → watcher disabled.
pub fn filewatcher_disabled() -> bool {
    truthy(
        std::env::var("OPENCODE_FILEWATCHER_DISABLE")
            .ok()
            .or_else(|| std::env::var("OPENCODE_DISABLE_FILEWATCHER").ok()),
    )
}

/// A `.opencode` candidate.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ProjectRoot {
    pub path: PathBuf,
    pub present: bool,
}

/// Eligible sources in priority order (farthest → nearest), including paths that may appear later.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Sources {
    pub global: Option<PathBuf>,
    pub explicit: Option<PathBuf>,
    pub direct: Vec<PathBuf>,
    pub project: Vec<ProjectRoot>,
    pub claude: Vec<PathBuf>,
    pub agents: Vec<PathBuf>,
}

/// `path.resolve` (lexical, against the process cwd).
pub fn absolute(path: &Path) -> PathBuf {
    let joined = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir().unwrap_or_else(|_| PathBuf::from("/")).join(path)
    };
    lexical(&joined)
}

/// Lexical normalization (`.`/`..` removal).
pub fn lexical(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for c in path.components() {
        match c {
            Component::ParentDir => {
                out.pop();
            }
            Component::CurDir => {}
            other => out.push(other.as_os_str()),
        }
    }
    if out.as_os_str().is_empty() { PathBuf::from("/") } else { out }
}

/// `FSUtil.resolve`: `path.resolve` then `realpath`, falling back to the unresolved path.
pub fn resolve(path: &Path) -> PathBuf {
    let resolved = absolute(path);
    std::fs::canonicalize(&resolved).unwrap_or(resolved)
}

/// `FSUtil.contains(parent, child)` (lexical).
pub fn contains(parent: &Path, child: &Path) -> bool {
    lexical(child).starts_with(lexical(parent))
}

fn is_dir(path: &Path) -> bool {
    std::fs::metadata(path).map(|m| m.is_dir()).unwrap_or(false)
}

/// `fs.up({ targets: ["."], start })`: every existing directory from `start` to the root.
pub fn walk_up(start: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let mut current = absolute(start);
    loop {
        if current.exists() {
            out.push(current.clone());
        }
        match current.parent() {
            Some(parent) if parent != current => current = parent.to_path_buf(),
            _ => break,
        }
    }
    out
}

fn dedupe(items: Vec<PathBuf>) -> Vec<PathBuf> {
    let mut out: Vec<PathBuf> = Vec::new();
    for item in items {
        if !out.contains(&item) {
            out.push(item);
        }
    }
    out
}

/// `ConfigDiscovery.discover`.
///
/// * `global_config` — global config dir `G` (honors `OPENCODE_CONFIG_DIR`);
/// * `home` — `Global.home` (honors `OPENCODE_TEST_HOME`);
/// * `directory` — the location's working directory `D`.
pub fn discover(global_config: &Path, home: &Path, directory: &Path, options: &Options) -> Sources {
    let global_directory = global_config.to_path_buf();
    let global_agents = home.join(".agents");
    let global_claude = home.join(".claude");
    let global_roots = [resolve(&global_directory), resolve(&global_claude), resolve(&global_agents)];
    let directories = if resolve(directory) == global_roots[0] || options.project == Some(false) {
        vec![]
    } else {
        walk_up(directory)
    };
    let mut discovered: Vec<(PathBuf, PathBuf)> = Vec::new();
    for dir in &directories {
        // Resolve the parent too: missing children must honor symlinked global roots.
        let parent = resolve(dir);
        for name in [".claude", ".agents", ".opencode", NAMES[1], NAMES[0]] {
            discovered.push((dir.join(name), resolve(&parent.join(name))));
        }
    }
    let global_enabled = options.global != Some(false);
    let global_files: Vec<PathBuf> = NAMES.iter().map(|n| resolve(&global_directory.join(n))).collect();
    // Global sources must not re-enter through the project walk.
    let visible: Vec<PathBuf> = discovered
        .into_iter()
        .filter(|(_, resolved)| {
            if global_enabled {
                !global_roots.contains(resolved) && !global_files.contains(resolved)
            } else {
                !global_roots.iter().any(|root| resolved == root || resolved.starts_with(root))
            }
        })
        .map(|(item, _)| item)
        .collect();
    let basename = |p: &PathBuf| p.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
    let named = |name: &str| -> Vec<PathBuf> {
        let mut items: Vec<PathBuf> = visible.iter().filter(|i| basename(i) == name).cloned().collect();
        items.reverse();
        items
    };
    let mut direct: Vec<PathBuf> =
        visible.iter().filter(|i| ![".agents", ".claude", ".opencode"].contains(&basename(i).as_str())).cloned().collect();
    direct.reverse();
    let project = named(".opencode").into_iter().map(|path| ProjectRoot { present: is_dir(&path), path }).collect();
    let mut claude = if global_enabled { vec![global_claude] } else { vec![] };
    claude.extend(named(".claude"));
    let mut agents = if global_enabled { vec![global_agents] } else { vec![] };
    agents.extend(named(".agents"));
    Sources {
        global: global_enabled.then_some(global_directory),
        explicit: options.file.as_deref().filter(|f| !f.is_empty()).map(|f| absolute(Path::new(f))),
        direct,
        project,
        claude: dedupe(claude),
        agents: dedupe(agents),
    }
}

impl Sources {
    /// `Config.compatibility()`: compatibility roots filtered to existing directories.
    pub fn compatibility(&self) -> (Vec<PathBuf>, Vec<PathBuf>) {
        (
            self.claude.iter().filter(|p| is_dir(p)).cloned().collect(),
            self.agents.iter().filter(|p| is_dir(p)).cloned().collect(),
        )
    }

    /// `ConfigWatch.plan`.
    pub fn watch_plan(&self) -> Vec<WatchTarget> {
        plan(self)
    }
}

/// Ignore globs for recursive directory watches.
pub const WATCH_IGNORE: [&str; 3] = ["node_modules", ".git", "**/{node_modules,.git}/**"];

/// Debounce applied to reload requests and to domain plugin rebuilds.
pub const DEBOUNCE_MS: u64 = 100;

/// Wellknown refresh period when any wellknown source exists.
pub const WELLKNOWN_REFRESH_MS: u64 = 10 * 60 * 1000;

/// One watch the core should maintain (`Watcher.WatchInput`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(untagged)]
pub enum WatchTarget {
    /// Recursive watch of a config root.
    Directory {
        path: PathBuf,
        #[serde(rename = "type")]
        kind: DirectoryKind,
        ignore: Vec<String>,
    },
    /// Non-recursive watch of `path` reporting only the listed child names.
    Entries {
        path: PathBuf,
        #[serde(rename = "type")]
        kind: EntriesKind,
        names: Vec<String>,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum DirectoryKind {
    Directory,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum EntriesKind {
    Entries,
}

impl WatchTarget {
    /// Reconciliation key (`JSON.stringify(target)`).
    pub fn key(&self) -> String {
        serde_json::to_string(self).unwrap_or_default()
    }
    pub fn path(&self) -> &Path {
        match self {
            WatchTarget::Directory { path, .. } | WatchTarget::Entries { path, .. } => path,
        }
    }
}

/// `ConfigWatch.plan`: recursive watches on the global dir and present `.opencode` dirs, plus a
/// parent `entries` watch for every other candidate not inside a watched directory.
pub fn plan(sources: &Sources) -> Vec<WatchTarget> {
    let mut directories: Vec<PathBuf> = sources.global.iter().cloned().collect();
    directories.extend(sources.project.iter().filter(|r| r.present).map(|r| r.path.clone()));
    let mut files: Vec<PathBuf> = sources.direct.clone();
    files.extend(sources.project.iter().map(|r| r.path.clone()));
    files.extend(sources.claude.iter().cloned());
    files.extend(sources.agents.iter().cloned());
    files.extend(sources.explicit.iter().cloned());
    let mut parents: Vec<(PathBuf, Vec<String>)> = Vec::new();
    for file in files {
        if directories.iter().any(|d| &file != d && contains(d, &file)) {
            continue;
        }
        let parent = file.parent().map(Path::to_path_buf).unwrap_or_else(|| PathBuf::from("/"));
        let name = file.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
        match parents.iter_mut().find(|(p, _)| p == &parent) {
            Some((_, names)) => names.push(name),
            None => parents.push((parent, vec![name])),
        }
    }
    let mut out: Vec<WatchTarget> = directories
        .into_iter()
        .map(|path| WatchTarget::Directory {
            path,
            kind: DirectoryKind::Directory,
            ignore: WATCH_IGNORE.iter().map(|s| s.to_string()).collect(),
        })
        .collect();
    for (path, names) in parents {
        let mut names = dedupe(names.into_iter().map(PathBuf::from).collect())
            .into_iter()
            .map(|p| p.to_string_lossy().to_string())
            .collect::<Vec<_>>();
        names.sort();
        out.push(WatchTarget::Entries { path, kind: EntriesKind::Entries, names });
    }
    // Map semantics: later duplicates replace earlier ones under the same key.
    let mut unique: Vec<WatchTarget> = Vec::new();
    for target in out {
        if let Some(i) = unique.iter().position(|t| t.key() == target.key()) {
            unique[i] = target;
        } else {
            unique.push(target);
        }
    }
    unique
}
