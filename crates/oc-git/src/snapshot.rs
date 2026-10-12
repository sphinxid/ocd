//! Port of `packages/core/src/snapshot.ts`: Location-scoped snapshots stored in a private Git
//! object store at `<data>/snapshot/<projectID>/<sha1(realpath(worktree))>`.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use futures::FutureExt;
use futures::future::{BoxFuture, Shared};
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};

use oc_schema::types::FileDiff;

use crate::git::{CaptureInput, DiffInput, Git, OperationError, Repository, TreeId};
use crate::paths;

/// Untracked files larger than this are not recorded.
pub const MAXIMUM_UNTRACKED_FILE_BYTES: u64 = 2 * 1024 * 1024;
/// Minimum time between pack checks.
pub const PACK_INTERVAL: Duration = Duration::from_secs(10 * 60);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SnapshotOperation {
    Capture,
    Files,
    Diff,
    Restore,
}

/// `Snapshot.Error`.
#[derive(Debug, Clone, thiserror::Error)]
#[error("{message}")]
pub struct SnapshotError {
    pub operation: SnapshotOperation,
    pub message: String,
}

impl SnapshotError {
    pub const TAG: &'static str = "Snapshot.Error";
    fn new(operation: SnapshotOperation, message: impl Into<String>) -> Self {
        SnapshotError { operation, message: message.into() }
    }
}

fn failure_from_snapshot(operation: SnapshotOperation, cause: &SnapshotError) -> SnapshotError {
    if cause.operation == operation {
        return cause.clone();
    }
    SnapshotError::new(operation, cause.message.clone())
}

fn failure_from_git(operation: SnapshotOperation, cause: &OperationError) -> SnapshotError {
    SnapshotError::new(operation, cause.message.clone())
}

/// Where a Snapshot service lives: the Location and its project.
#[derive(Debug, Clone)]
pub struct SnapshotLocation {
    /// `Global.data` (`<XDG_DATA_HOME>/opencode`).
    pub data: PathBuf,
    pub project_id: String,
    /// `location.project.directory` (the repository worktree root).
    pub project_directory: PathBuf,
    /// `location.directory` (possibly a subdirectory of the project).
    pub directory: PathBuf,
    /// `location.vcs?.type`; snapshots run only for `"git"`.
    pub vcs: Option<String>,
}

#[derive(Debug, Clone)]
pub struct SnapshotRepository {
    pub source: Repository,
    pub worktree: PathBuf,
    pub snapshot_repository: Repository,
}

type RepoFuture = Shared<BoxFuture<'static, Result<SnapshotRepository, SnapshotError>>>;

struct Inner {
    git: Git,
    location: SnapshotLocation,
    enabled: AtomicBool,
    repository: Mutex<Option<RepoFuture>>,
    last_pack_check: Mutex<Option<Instant>>,
}

/// The `Snapshot` service for one Location.
#[derive(Clone)]
pub struct Snapshots {
    inner: Arc<Inner>,
}

/// `<data>/snapshot/<projectID>/<sha1(worktree)>`.
pub fn store_directory(data: &Path, project_id: &str, worktree: &Path) -> PathBuf {
    data.join("snapshot").join(project_id).join(oc_util::hash::fast(worktree.to_string_lossy().as_bytes()))
}

impl Snapshots {
    pub fn new(git: Git, location: SnapshotLocation) -> Self {
        Snapshots {
            inner: Arc::new(Inner {
                git,
                location,
                enabled: AtomicBool::new(true),
                repository: Mutex::new(None),
                last_pack_check: Mutex::new(None),
            }),
        }
    }

    /// State editor `configure(enabled)`.
    pub fn configure(&self, enabled: bool) {
        self.inner.enabled.store(enabled, Ordering::SeqCst);
    }

    fn enabled(&self) -> bool {
        self.inner.location.vcs.as_deref() == Some("git") && self.inner.enabled.load(Ordering::SeqCst)
    }

    /// Lazily discovered and cached (including failure). Runs in its own task so a cancelled
    /// caller never poisons the shared initialization.
    pub async fn repository(&self) -> Result<SnapshotRepository, SnapshotError> {
        let future = {
            let mut slot = self.inner.repository.lock();
            slot.get_or_insert_with(|| {
                let inner = self.inner.clone();
                let handle = tokio::spawn(async move { initialize(&inner.git, &inner.location).await });
                async move {
                    handle.await.unwrap_or_else(|error| Err(SnapshotError::new(SnapshotOperation::Capture, error.to_string())))
                }
                .boxed()
                .shared()
            })
            .clone()
        };
        future.await
    }

    fn scope(&self, worktree: &Path) -> Result<String, SnapshotError> {
        let directory = &self.inner.location.directory;
        if !paths::contains(worktree, directory) {
            return Err(SnapshotError::new(SnapshotOperation::Capture, "Location is outside the project"));
        }
        let relative = paths::relative(worktree, directory).replace('\\', "/");
        Ok(if relative.is_empty() { ".".to_string() } else { relative })
    }

    /// `objects.pack` takes a cross-process lock, so a run that outlasts the interval never overlaps the next.
    fn pack_when_due(&self, repository: &Repository) {
        {
            let mut last = self.inner.last_pack_check.lock();
            let now = Instant::now();
            if last.is_some_and(|last| now.duration_since(last) < PACK_INTERVAL) {
                return;
            }
            *last = Some(now);
        }
        let git = self.inner.git.clone();
        let repository = repository.clone();
        tokio::spawn(async move {
            if let Err(cause) = git.pack(&repository).await {
                tracing::warn!(cause = %cause, "failed to pack snapshot objects");
            }
        });
    }

    /// Capture the current Location-scoped filesystem state as a content-addressed tree. Returns
    /// `None` when snapshots are disabled, unsupported, or the best-effort capture fails.
    pub async fn capture(&self) -> Option<TreeId> {
        if !self.enabled() {
            return None;
        }
        let result: Result<TreeId, String> = async {
            let repo = self.repository().await.map_err(|e| e.message)?;
            let scope = self.scope(&repo.worktree).map_err(|e| e.message)?;
            let tree = self
                .inner
                .git
                .capture(&CaptureInput {
                    repository: repo.snapshot_repository.clone(),
                    scopes: vec![scope],
                    ignores: Some(repo.source.clone()),
                    maximum_untracked_file_bytes: Some(MAXIMUM_UNTRACKED_FILE_BYTES),
                })
                .await
                .map_err(|e| e.message)?;
            self.pack_when_due(&repo.snapshot_repository);
            Ok(tree)
        }
        .await;
        match result {
            Ok(tree) => Some(tree),
            Err(cause) => {
                tracing::warn!(cause = %cause, "failed to capture snapshot");
                None
            }
        }
    }

    async fn comparison(&self, operation: SnapshotOperation) -> Result<SnapshotRepository, SnapshotError> {
        self.repository().await.map_err(|cause| failure_from_snapshot(operation, &cause))
    }

    /// Snapshots track every scoped file; the source repository's ignore rules decide what callers see.
    async fn ignored(
        &self,
        operation: SnapshotOperation,
        source: &Repository,
        paths: &[String],
    ) -> Result<std::collections::HashSet<String>, SnapshotError> {
        self.inner.git.ignored(source, paths).await.map_err(|cause| failure_from_git(operation, &cause))
    }

    /// List project-relative paths changed between two captured trees.
    pub async fn files(&self, from: &TreeId, to: &TreeId) -> Result<Vec<String>, SnapshotError> {
        let compared = self.comparison(SnapshotOperation::Files).await?;
        let changed = self
            .inner
            .git
            .tree_files(&compared.snapshot_repository, from, to)
            .await
            .map_err(|cause| failure_from_git(SnapshotOperation::Files, &cause))?;
        let skipped = self.ignored(SnapshotOperation::Files, &compared.source, &changed).await?;
        Ok(changed.into_iter().filter(|file| !skipped.contains(file)).collect())
    }

    /// Structured per-file diffs between two captured trees. `context` controls unchanged lines
    /// around each hunk (default 3).
    pub async fn diff(
        &self,
        from: &TreeId,
        to: &TreeId,
        context: Option<u64>,
        paths: Option<Vec<String>>,
    ) -> Result<Vec<FileDiff>, SnapshotError> {
        if paths.as_ref().is_some_and(|p| p.is_empty()) {
            return Ok(Vec::new());
        }
        let compared = self.comparison(SnapshotOperation::Diff).await?;
        // Only an explicit selection becomes a pathspec; ignored paths are dropped from the result instead.
        let diffs = self
            .inner
            .git
            .tree_diff(&compared.snapshot_repository, &DiffInput { from: from.clone(), to: to.clone(), context, paths })
            .await
            .map_err(|cause| failure_from_git(SnapshotOperation::Diff, &cause))?;
        let files: Vec<String> = diffs.iter().map(|d| d.file.clone()).collect();
        let skipped = self.ignored(SnapshotOperation::Diff, &compared.source, &files).await?;
        Ok(diffs.into_iter().filter(|d| !skipped.contains(&d.file)).collect())
    }

    /// Restore selected project-relative paths from their associated trees. A path absent from its
    /// selected tree is removed; paths outside the list are untouched.
    pub async fn restore(&self, files: &[(String, TreeId)]) -> Result<(), SnapshotError> {
        if !self.enabled() {
            return Err(SnapshotError::new(SnapshotOperation::Restore, "Snapshots are disabled"));
        }
        let repo = self.repository().await.map_err(|cause| failure_from_snapshot(SnapshotOperation::Restore, &cause))?;
        let mut plan: Vec<(String, TreeId)> = Vec::new();
        for (file, snapshot) in files {
            let absolute = paths::resolve(&repo.worktree, file);
            if !paths::contains(&repo.worktree, &absolute) {
                return Err(SnapshotError::new(SnapshotOperation::Restore, format!("Path escapes the project: {file}")));
            }
            // A JS Map keeps the first insertion position and the last value for repeated keys.
            match plan.iter_mut().find(|(f, _)| f == file) {
                Some(entry) => entry.1 = snapshot.clone(),
                None => plan.push((file.clone(), snapshot.clone())),
            }
        }
        self.inner
            .git
            .restore(&repo.snapshot_repository, &plan)
            .await
            .map_err(|cause| failure_from_git(SnapshotOperation::Restore, &cause))
    }

    /// Run a pack check now (the same work `capture` schedules at most every 10 minutes).
    pub async fn pack(&self) -> Result<(), SnapshotError> {
        let repo = self.repository().await?;
        self.inner
            .git
            .pack(&repo.snapshot_repository)
            .await
            .map_err(|cause| failure_from_git(SnapshotOperation::Capture, &cause))
    }
}

async fn initialize(git: &Git, location: &SnapshotLocation) -> Result<SnapshotRepository, SnapshotError> {
    let Some(source) = git.discover(&location.project_directory).await else {
        return Err(SnapshotError::new(SnapshotOperation::Capture, "Project is not a Git repository"));
    };
    let worktree = tokio::fs::canonicalize(&source.worktree)
        .await
        .map_err(|cause| SnapshotError::new(SnapshotOperation::Capture, cause.to_string()))?;
    let git_directory = store_directory(&location.data, &location.project_id, &worktree);
    let snapshot_repository = if tokio::fs::try_exists(git_directory.join("HEAD")).await.unwrap_or(false) {
        Repository { worktree: worktree.clone(), git_directory: git_directory.clone(), common_directory: git_directory }
    } else {
        git.create(&worktree, &git_directory, Some(&source))
            .await
            .map_err(|cause| failure_from_git(SnapshotOperation::Capture, &cause))?
    };
    Ok(SnapshotRepository { source, worktree, snapshot_repository })
}
