//! A location's cached VCS info and its branch watcher (`core/src/vcs.ts` `refresh`,
//! `core/src/filesystem/location-watcher.ts`).
//!
//! `vcs.get` answers the info cached by the last refresh. A refresh runs when the location boots
//! (the Vcs State notify), when plugins change the `vcs` domain, and when the branch metadata
//! changes on disk: git's `HEAD` inside the repository's git directory, or Mercurial's
//! `.hg/branch`. The file is watched once plugins have activated, unless the config's
//! `watcher.ignore` names `.git`/`.hg` or the store, or the file watcher is disabled. Each change
//! is announced as `filesystem.changed`, and a branch that differs from the cached one as
//! `vcs.branch.updated`.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Weak};

use oc_core::app::App;
use oc_git::Git;
use oc_git::vcs::Info;
use oc_schema::types::LocationRef;
use parking_lot::Mutex;
use serde_json::{Value, json};

use crate::locations::Loc;

#[derive(Default)]
pub struct VcsState {
    /// `current.info`; `None` until the first refresh.
    info: Mutex<Option<Info>>,
    /// `refreshLock`.
    refresh: tokio::sync::Mutex<()>,
    watcher: Mutex<Option<notify::RecommendedWatcher>>,
}

impl VcsState {
    /// The info of the last refresh.
    pub fn cached(&self) -> Option<Info> {
        self.info.lock().clone()
    }

    /// Carry the info over from the services a configuration reload replaced.
    pub fn seed(&self, info: Option<Info>) {
        *self.info.lock() = info;
    }

    fn branch(&self) -> Option<String> {
        self.info.lock().as_ref().and_then(|i| i.branch.current.clone())
    }
}

fn at(loc: &Loc) -> Option<LocationRef> {
    Some(LocationRef { directory: loc.key.clone(), workspace_id: None })
}

/// The selected provider's info over the location's scope (`{branch:{}}` without one).
pub async fn info(app: &App, loc: &Loc) -> Info {
    let registry = app.plugins.vcs(&loc.key, loc.vcs()).await;
    oc_core::vcs::Vcs { provider: registry.selected(loc.vcs()), scope: oc_core::vcs::Scope::of(&loc.location) }
        .info()
        .await
}

/// `Vcs.refresh`: re-read the info and announce the branch when it changed.
pub async fn refresh(app: &App, loc: &Loc) {
    let state = &loc.vcs_state;
    let changed = {
        let _permit = state.refresh.lock().await;
        let next = info(app, loc).await;
        let mut current = state.info.lock();
        let changed = current.as_ref().and_then(|i| i.branch.current.as_ref()) != next.branch.current.as_ref();
        *current = Some(next);
        changed
    };
    if !changed {
        return;
    }
    // Re-announce the latest branch if a nested refresh overtook this publication.
    loop {
        let branch = state.branch();
        let data = match &branch {
            Some(branch) => json!({ "branch": branch }),
            None => json!({}),
        };
        app.bus.publish("vcs.branch.updated", data, at(loc));
        if branch == state.branch() {
            return;
        }
    }
}

/// The file whose changes mean the branch changed, and the `watcher.ignore` entries that turn the
/// watch off (`LocationWatcher` `target`).
struct Target {
    path: PathBuf,
    aliases: Vec<String>,
}

async fn target(git: &Git, loc: &Loc) -> Option<Target> {
    match loc.location.project.vcs.as_ref()? {
        oc_core::project::Vcs::Git { .. } => {
            let resolved = git.discover(loc.directory()).await?.git_directory;
            let vcs = realpath(&resolved);
            Some(Target {
                path: vcs.join("HEAD"),
                aliases: vec![".git".into(), vcs.display().to_string(), resolved.display().to_string()],
            })
        }
        oc_core::project::Vcs::Hg { store } => {
            let vcs = realpath(store);
            Some(Target { path: vcs.join("branch"), aliases: vec![".hg".into(), vcs.display().to_string()] })
        }
    }
}

fn realpath(path: &Path) -> PathBuf {
    std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
}

/// `isBranchMetadata`: git's `HEAD` anywhere under the store, or the store's hg `branch` file.
fn is_branch_metadata(loc: &Loc, file: &Path) -> bool {
    match loc.location.project.vcs.as_ref() {
        Some(oc_core::project::Vcs::Git { store }) => {
            file.file_name().is_some_and(|n| n == "HEAD") && file.starts_with(realpath(store))
        }
        Some(oc_core::project::Vcs::Hg { store }) => file == realpath(store).join("branch"),
        None => false,
    }
}

/// Start the location's branch watcher (after plugin activation, like `LocationWatcher`).
pub async fn watch(app: &Arc<App>, git: &Git, loc: &Arc<Loc>) {
    if oc_config::discovery::filewatcher_disabled() {
        return;
    }
    let Some(target) = target(git, loc).await else { return };
    let ignore = oc_config::fold::watcher_ignore(&loc.location.entries);
    if target.aliases.iter().any(|alias| ignore.contains(alias)) {
        return;
    }
    let (Some(directory), Some(name)) = (target.path.parent().map(Path::to_path_buf), target.path.file_name().map(|n| n.to_owned()))
    else {
        return;
    };
    let Ok(handle) = tokio::runtime::Handle::try_current() else { return };
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<PathBuf>();
    // Like Node's `fs.watch` on the directory, filtered to the one name: each create, write,
    // rename-into-place or delete of the file is one update.
    let watched = target.path.clone();
    let created = notify::recommended_watcher(move |event: notify::Result<notify::Event>| {
        let Ok(event) = event else { return };
        if matches!(
            event.kind,
            notify::EventKind::Access(_) | notify::EventKind::Modify(notify::event::ModifyKind::Name(notify::event::RenameMode::Both))
        ) {
            return;
        }
        if event.paths.iter().any(|p| p.file_name() == Some(name.as_os_str())) {
            let _ = tx.send(watched.clone());
        }
    });
    let mut watcher = match created {
        Ok(watcher) => watcher,
        Err(error) => {
            tracing::error!(path = %target.path.display(), %error, "location watcher subscription failed");
            return;
        }
    };
    {
        use notify::Watcher;
        if let Err(error) = watcher.watch(&directory, notify::RecursiveMode::NonRecursive) {
            tracing::error!(path = %target.path.display(), %error, "location watcher subscription failed");
            return;
        }
    }
    tracing::info!(path = %target.path.display(), r#type = "file", backend = "node", "watcher started");
    *loc.vcs_state.watcher.lock() = Some(watcher);
    let (app, weak): (Arc<App>, Weak<Loc>) = (app.clone(), Arc::downgrade(loc));
    handle.spawn(async move {
        while let Some(file) = rx.recv().await {
            let Some(loc) = weak.upgrade() else { break };
            let data: Value = json!({ "file": file.display().to_string(), "event": "change" });
            app.bus.publish("filesystem.changed", data, at(&loc));
            if is_branch_metadata(&loc, &file) {
                refresh(&app, &loc).await;
            }
        }
    });
}

