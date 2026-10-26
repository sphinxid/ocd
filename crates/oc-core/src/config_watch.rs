//! Config file watching (`config.ts:240-327`, `config/watch.ts`): watch the location's config sources and
//! request a debounced reload whenever one of them changes.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use notify::{RecursiveMode, Watcher};
use oc_config::discovery::{WatchTarget, DEBOUNCE_MS};
use tokio::sync::mpsc;

/// A set of filesystem watches reconciled against `ConfigWatch.plan`.
pub struct ConfigWatch {
    watcher: notify::RecommendedWatcher,
    /// key → target
    active: HashMap<String, WatchTarget>,
    targets: Arc<parking_lot::RwLock<Vec<WatchTarget>>>,
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

impl ConfigWatch {
    /// Start watching; every reported change sends `()` on `changes`.
    pub fn new(changes: mpsc::UnboundedSender<()>) -> notify::Result<Self> {
        let targets: Arc<parking_lot::RwLock<Vec<WatchTarget>>> = Arc::default();
        let filter = targets.clone();
        let watcher = notify::recommended_watcher(move |event: notify::Result<notify::Event>| {
            let Ok(event) = event else { return };
            if matches!(event.kind, notify::EventKind::Access(_)) {
                return;
            }
            let targets = filter.read();
            if event.paths.iter().any(|p| targets.iter().any(|t| reports(t, p))) {
                let _ = changes.send(());
            }
        })?;
        Ok(ConfigWatch { watcher, active: HashMap::new(), targets })
    }

    /// Reconcile watches (keys = JSON of target; removed when no longer planned).
    pub fn reconcile(&mut self, plan: Vec<WatchTarget>) {
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
        *self.targets.write() = plan;
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
