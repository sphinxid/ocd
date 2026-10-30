//! `FileWatch` (`core/src/filesystem/file-watch.ts`): single files watched over the API
//! (`POST`/`DELETE /api/experimental/fs/watch`). Global rather than per location, so watches
//! outlive location eviction. Each change publishes `filesystem.changed` on the location.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use indexmap::IndexMap;
use oc_core::bus::Bus;
use oc_schema::types::LocationRef;
use parking_lot::Mutex;
use serde_json::json;

/// How long a watch lives without being renewed.
pub const TTL_MS: i64 = 24 * 60 * 60 * 1000;
/// How often expired watches are stopped.
pub const SWEEP_INTERVAL: Duration = Duration::from_secs(5 * 60);
/// Watches per location; the one expiring soonest is stopped to make room.
pub const MAX_PER_LOCATION: usize = 256;

/// `FileWatch.DisabledError`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DisabledError;

impl std::fmt::Display for DisabledError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("File watching is disabled")
    }
}

impl std::error::Error for DisabledError {}

struct Entry {
    id: u64,
    expires: i64,
    _watcher: notify::RecommendedWatcher,
    task: tokio::task::JoinHandle<()>,
}

impl Drop for Entry {
    fn drop(&mut self) {
        self.task.abort();
    }
}

type Locations = IndexMap<String, IndexMap<PathBuf, Entry>>;

pub struct FileWatch {
    bus: Bus,
    enabled: bool,
    locations: Arc<Mutex<Locations>>,
    ids: std::sync::atomic::AtomicU64,
    sweeper: std::sync::OnceLock<tokio::task::JoinHandle<()>>,
}

fn now_ms() -> i64 {
    oc_schema::id::now_ms() as i64
}

/// `[location.directory, location.workspaceID]`.
fn location_key(location: &LocationRef) -> String {
    serde_json::to_string(&json!([location.directory, location.workspace_id])).unwrap_or_default()
}

/// Forget an entry if it is still the one with `id` (`None` matches any).
fn remove(locations: &mut Locations, key: &str, file: &Path, id: Option<u64>) -> Option<Entry> {
    let files = locations.get_mut(key)?;
    if id.is_some_and(|id| files.get(file).is_none_or(|entry| entry.id != id)) {
        return None;
    }
    let entry = files.shift_remove(file);
    if files.is_empty() {
        locations.shift_remove(key);
    }
    entry
}

impl FileWatch {
    /// Disabled by `OPENCODE_FILEWATCHER_DISABLE` / `OPENCODE_DISABLE_FILEWATCHER`.
    pub fn new(bus: Bus) -> Self {
        Self::with_enabled(bus, !oc_config::discovery::filewatcher_disabled())
    }

    pub fn with_enabled(bus: Bus, enabled: bool) -> Self {
        FileWatch {
            bus,
            enabled,
            locations: Arc::new(Mutex::new(IndexMap::new())),
            ids: std::sync::atomic::AtomicU64::new(0),
            sweeper: std::sync::OnceLock::new(),
        }
    }

    /// Number of files watched for a location (tests).
    pub fn watched(&self, location: &LocationRef) -> Vec<PathBuf> {
        self.locations
            .lock()
            .get(&location_key(location))
            .map(|files| files.keys().cloned().collect())
            .unwrap_or_default()
    }

    fn start_sweeper(&self) {
        let Ok(handle) = tokio::runtime::Handle::try_current() else { return };
        let locations = Arc::downgrade(&self.locations);
        self.sweeper.get_or_init(|| {
            handle.spawn(async move {
                loop {
                    tokio::time::sleep(SWEEP_INTERVAL).await;
                    let Some(locations) = locations.upgrade() else { return };
                    let now = now_ms();
                    let expired: Vec<Entry> = {
                        let mut locations = locations.lock();
                        let keys: Vec<(String, PathBuf)> = locations
                            .iter()
                            .flat_map(|(key, files)| {
                                files
                                    .iter()
                                    .filter(|(_, e)| e.expires <= now)
                                    .map(|(file, _)| (key.clone(), file.clone()))
                            })
                            .collect();
                        keys.into_iter().filter_map(|(key, file)| remove(&mut locations, &key, &file, None)).collect()
                    };
                    drop(expired);
                }
            })
        });
    }

    /// Starts watching `file` (absolute), or resets its expiry when it is already watched.
    pub fn watch(&self, location: &LocationRef, file: &Path) -> Result<(), DisabledError> {
        if !self.enabled {
            return Err(DisabledError);
        }
        self.start_sweeper();
        let key = location_key(location);
        let expires = now_ms() + TTL_MS;
        // Held throughout, so a watch that ends at once is only forgotten after it was recorded.
        let mut locations = self.locations.lock();
        if let Some(entry) = locations.get_mut(&key).and_then(|files| files.get_mut(file)) {
            entry.expires = expires;
            return Ok(());
        }
        if let Some(files) = locations.get(&key).filter(|files| files.len() >= MAX_PER_LOCATION) {
            // `reduce` keeps the first of equally soon entries.
            let soonest = files
                .iter()
                .fold(None::<(&PathBuf, i64)>, |best, (path, entry)| match best {
                    Some((_, expires)) if entry.expires >= expires => best,
                    _ => Some((path, entry.expires)),
                })
                .map(|(path, _)| path.clone());
            if let Some(soonest) = soonest {
                drop(remove(&mut locations, &key, &soonest, None));
            }
        }
        let id = self.ids.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let Some((watcher, task)) = self.start(&key, location, file, id) else { return Ok(()) };
        locations.entry(key).or_default().insert(file.to_path_buf(), Entry { id, expires, _watcher: watcher, task });
        Ok(())
    }

    /// Stops watching `file`; no error when it is not watched.
    pub fn unwatch(&self, location: &LocationRef, file: &Path) {
        let entry = remove(&mut self.locations.lock(), &location_key(location), file, None);
        drop(entry);
    }

    /// Subscribes to the file's directory, filtered to its name. A watch that fails or ends (its
    /// directory vanished) is logged and forgotten, so the next `watch` starts a new one.
    fn start(
        &self,
        key: &str,
        location: &LocationRef,
        file: &Path,
        id: u64,
    ) -> Option<(notify::RecommendedWatcher, tokio::task::JoinHandle<()>)> {
        let handle = tokio::runtime::Handle::try_current().ok()?;
        let failed = |error: &dyn std::fmt::Display| {
            tracing::error!(file = %file.display(), cause = %error, "file watch failed");
        };
        let (Some(directory), Some(name)) =
            (file.parent().map(Path::to_path_buf), file.file_name().map(|n| n.to_owned()))
        else {
            failed(&"not a file path");
            return None;
        };
        enum Update {
            Changed,
            Ended,
        }
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<Update>();
        let watched_directory = directory.clone();
        let created = notify::recommended_watcher(move |event: notify::Result<notify::Event>| {
            let Ok(event) = event else {
                let _ = tx.send(Update::Ended);
                return;
            };
            if matches!(
                event.kind,
                notify::EventKind::Access(_)
                    | notify::EventKind::Modify(notify::event::ModifyKind::Name(notify::event::RenameMode::Both))
            ) {
                return;
            }
            if matches!(event.kind, notify::EventKind::Remove(_)) && event.paths.iter().any(|p| *p == watched_directory)
            {
                let _ = tx.send(Update::Ended);
                return;
            }
            if event.paths.iter().any(|p| p.file_name() == Some(name.as_os_str())) {
                let _ = tx.send(Update::Changed);
            }
        });
        let mut watcher = match created {
            Ok(watcher) => watcher,
            Err(error) => {
                failed(&error);
                return None;
            }
        };
        {
            use notify::Watcher;
            if let Err(error) = watcher.watch(&directory, notify::RecursiveMode::NonRecursive) {
                failed(&error);
                return None;
            }
        }
        let bus = self.bus.clone();
        let locations = Arc::downgrade(&self.locations);
        let location = location.clone();
        let file = file.to_path_buf();
        let key = key.to_string();
        let task = handle.spawn(async move {
            while let Some(update) = rx.recv().await {
                if matches!(update, Update::Ended) {
                    tracing::error!(file = %file.display(), "file watch failed");
                    break;
                }
                let exists = tokio::fs::try_exists(&file).await.unwrap_or(false);
                let data =
                    json!({ "file": file.display().to_string(), "event": if exists { "change" } else { "unlink" } });
                bus.publish("filesystem.changed", data, Some(location.clone()));
            }
            // The watch ended: forget it (outside this task, since dropping the entry aborts it).
            if let Some(locations) = locations.upgrade() {
                let entry = remove(&mut locations.lock(), &key, &file, Some(id));
                if let Some(entry) = entry {
                    tokio::spawn(async move { drop(entry) });
                }
            }
        });
        Some((watcher, task))
    }
}

impl Drop for FileWatch {
    fn drop(&mut self) {
        if let Some(sweeper) = self.sweeper.get() {
            sweeper.abort();
        }
    }
}
