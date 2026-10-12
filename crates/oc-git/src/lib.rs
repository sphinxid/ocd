//! Git operations for opencode: repository discovery, snapshots (capture/files/diff/restore/pack),
//! worktrees, the remote repository cache, and file retention sweeps.
//!
//! Every operation shells out to the `git` binary exactly like upstream `packages/core/src/git.ts`.

pub mod file_retention;
pub mod git;
pub mod patch;
pub mod paths;
pub mod process;
pub mod repository;
pub mod repository_cache;
pub mod snapshot;
pub mod vcs;
pub mod worktree;

pub use git::{
    CaptureInput, CheckoutInput, CloneInput, DiffInput, FetchBranchInput, Git, Operation, OperationError,
    RefreshResult, Repository, TreeId, Worktree, WorktreeError, WorktreeKind, WorktreeOperation,
};
pub use repository::{Reference, RepositoryError};
pub use repository_cache::{CacheError, EnsureInput, EnsureResult, Kv, MemoryKv, Refresh, RepositoryCache, Status};
pub use snapshot::{SnapshotError, SnapshotLocation, SnapshotOperation, Snapshots};
pub use vcs::GitVcs;
pub use worktree::{GitWorktreeStrategy, ListEntry, ListEntryType, WorktreeStrategyError};
