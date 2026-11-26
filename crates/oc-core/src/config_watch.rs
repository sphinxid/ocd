//! Config file watching (`config.ts:240-327`, `config/watch.ts`): watch the location's config sources and
//! request a debounced reload whenever a config input among them changes (2.4.1, #62158).

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use notify::{RecursiveMode, Watcher};
use oc_config::discovery::{Inputs, WatchTarget, DEBOUNCE_MS};
use tokio::sync::mpsc;

/// A set of filesystem watches reconciled against `ConfigWatch.plan`.
pub struct ConfigWatch {
    watcher: notify::RecommendedWatcher,
    /// key → target
    active: HashMap<String, WatchTarget>,
    /// Each planned target with its real path, and the paths whose changes feed the config.
    filter: Arc<parking_lot::RwLock<(Vec<(WatchTarget, PathBuf)>, Inputs)>>,
}

/// `FSUtil.ignoreGlobs`: a path inside an ignored directory (`node_modules`, `.git`, `.venv`, `venv`,
/// `__pycache__`) at any depth.
fn ignored(path: &Path) -> bool {
    path.components()
        .any(|c| c.as_os_str().to_str().is_some_and(|name| oc_config::discovery::IGNORED_DIRECTORIES.contains(&name)))
}

/// Whether a changed path is reported by `target`.
pub fn reports(target: &WatchTarget, path: &Path) -> bool {
    match target {
        WatchTarget::Directory { path: root, .. } => {
            path.strip_prefix(root).is_ok_and(|rel| !ignored(rel)) || path == root.as_path()
        }
        WatchTarget::Entries { path: parent, names, .. } => {
            // Parent watches make deletion/re-creation of the listed children observable.
            path.strip_prefix(parent).ok().and_then(|rel| rel.components().next()).is_some_and(|first| {
                let first = first.as_os_str().to_string_lossy();
                names.iter().any(|n| *n == first)
            })
        }
    }
}

/// Whether a change at `path` reloads the config: it is reported by a target and is a config input. Native
/// watches can report the canonical path even after the root was deleted, so a path under a target's real path
/// counts as the same path under the target.
pub fn reloads(targets: &[(WatchTarget, PathBuf)], inputs: &Inputs, path: &Path) -> bool {
    targets.iter().any(|(target, real)| {
        if reports(target, path) && inputs.contains(path) {
            return true;
        }
        if real == target.path() {
            return false;
        }
        let Ok(relative) = path.strip_prefix(real) else { return false };
        let translated = if relative.as_os_str().is_empty() { target.path().to_path_buf() } else { target.path().join(relative) };
        reports(target, &translated) && inputs.contains(&translated)
    })
}

impl ConfigWatch {
    /// Start watching; every change to a config input sends `()` on `changes`.
    pub fn new(changes: mpsc::UnboundedSender<()>) -> notify::Result<Self> {
        let filter: Arc<parking_lot::RwLock<(Vec<(WatchTarget, PathBuf)>, Inputs)>> = Arc::default();
        let reading = filter.clone();
        let watcher = notify::recommended_watcher(move |event: notify::Result<notify::Event>| {
            let Ok(event) = event else { return };
            if matches!(event.kind, notify::EventKind::Access(_)) {
                return;
            }
            let filter = reading.read();
            if event.paths.iter().any(|p| reloads(&filter.0, &filter.1, p)) {
                let _ = changes.send(());
            }
        })?;
        Ok(ConfigWatch { watcher, active: HashMap::new(), filter })
    }

    /// Reconcile watches (keys = JSON of target; removed when no longer planned), and the config inputs of the
    /// latest load.
    pub fn reconcile(&mut self, plan: Vec<WatchTarget>, inputs: Inputs) {
        let keys: HashMap<String, &WatchTarget> = plan.iter().map(|t| (t.key(), t)).collect();
        let stale: Vec<String> = self.active.keys().filter(|k| !keys.contains_key(*k)).cloned().collect();
        for key in stale {
            if let Some(target) = self.active.remove(&key) {
                let _ = self.watcher.unwatch(target.path());
            }
        }
        for (key, target) in keys {
            if self.active.contains_key(&key) {
                continue;
            }
            let mode = match target {
                WatchTarget::Directory { .. } => RecursiveMode::Recursive,
                WatchTarget::Entries { .. } => RecursiveMode::NonRecursive,
            };
            match self.watcher.watch(target.path(), mode) {
                Ok(()) => {
                    self.active.insert(key, target.clone());
                }
                Err(error) => tracing::debug!(path = %target.path().display(), error = %error, "config watch failed"),
            }
        }
        // `FSUtil.resolve`: the real path a native watch may report instead.
        let targets = plan.into_iter().map(|t| {
            let real = oc_util::fs::canonicalize(t.path()).unwrap_or_else(|_| t.path().to_path_buf());
            (t, real)
        });
        *self.filter.write() = (targets.collect(), inputs);
    }

    pub fn paths(&self) -> Vec<PathBuf> {
        self.active.values().map(|t| t.path().to_path_buf()).collect()
    }
}

/// Collapse bursts: wait for the first change, then until `DEBOUNCE_MS` passes without another.
pub async fn debounced(rx: &mut mpsc::UnboundedReceiver<()>) -> Option<()> {
    rx.recv().await?;
    loop {
        match tokio::time::timeout(Duration::from_millis(DEBOUNCE_MS), rx.recv()).await {
            Ok(Some(())) => continue,
            Ok(None) => return Some(()),
            Err(_) => return Some(()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use oc_config::discovery::{DirectoryKind, EntriesKind};

    /// config.test.ts (2.4.1, #62158): only config inputs reload, events for a symlinked root arrive under its real
    /// path (even after its removal), and inputs match without case.
    #[test]
    fn reloads_only_config_inputs() {
        use oc_config::discovery::{ProjectRoot, Sources};
        let sources = Sources {
            global: Some("/g".into()),
            project: vec![ProjectRoot { path: "/p/.opencode".into(), present: true }],
            ..Default::default()
        };
        let references = [PathBuf::from("/g/opencode.json"), PathBuf::from("/g/work.json")].into_iter().collect();
        let inputs = oc_config::discovery::inputs(&sources, &references);
        let dir = |path: &str| WatchTarget::Directory { path: path.into(), kind: DirectoryKind::Directory, ignore: vec![] };
        let targets = vec![(dir("/g"), PathBuf::from("/g")), (dir("/p/.opencode"), PathBuf::from("/real/.opencode"))];
        let reloads = |path: &str| reloads(&targets, &inputs, Path::new(path));
        assert!(reloads("/g/opencode.json"));
        assert!(reloads("/g/OpenCode.JSON"));
        // A `{file:}` reference under a watched root.
        assert!(reloads("/g/work.json"));
        // Unrelated paths under a config root.
        assert!(!reloads("/g/skills/demo/SKILL.md"));
        assert!(!reloads("/g/opencode.json.bak"));
        // Canonical events for the symlinked project root, a file in it and the root itself.
        assert!(reloads("/real/.opencode/opencode.jsonc"));
        assert!(reloads("/real/.opencode"));
        assert!(!reloads("/real/.opencode/agent/x.md"));
        assert!(!reloads("/elsewhere/opencode.json"));
    }

    #[test]
    fn report_rules() {
        let dir = WatchTarget::Directory { path: "/p/.opencode".into(), kind: DirectoryKind::Directory, ignore: vec![] };
        assert!(reports(&dir, Path::new("/p/.opencode/agent/x.md")));
        assert!(!reports(&dir, Path::new("/p/.opencode/node_modules/a/b.js")));
        assert!(!reports(&dir, Path::new("/p/.opencode/skills/x/.venv/lib/a.py")));
        assert!(!reports(&dir, Path::new("/p/.opencode/__pycache__/a.pyc")));
        assert!(!reports(&dir, Path::new("/p/other")));
        let entries = WatchTarget::Entries {
            path: "/p".into(),
            kind: EntriesKind::Entries,
            names: vec!["opencode.json".into(), ".claude".into()],
        };
        assert!(reports(&entries, Path::new("/p/opencode.json")));
        assert!(reports(&entries, Path::new("/p/.claude")));
        assert!(!reports(&entries, Path::new("/p/src/main.rs")));
    }
}
