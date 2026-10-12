//! Port of `packages/core/src/repository-cache.ts`: local tracking checkouts for remote Git
//! references, one per remote and branch, refreshed "newest wins" (fetch + hard reset).

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::git::{CheckoutInput, CloneInput, FetchBranchInput, Git};
use crate::paths;
use crate::repository::{self, Reference, RepositoryError};

pub const REFRESH_INTERVAL_MS: f64 = 24.0 * 60.0 * 60.0 * 1000.0;

/// The persisted key/value store (`KV.Service`, table `kv`). Values are JSON.
pub trait Kv: Send + Sync {
    fn get(&self, key: &str) -> anyhow::Result<Option<Value>>;
    fn set(&self, key: &str, value: &Value) -> anyhow::Result<()>;
}

/// In-memory [`Kv`], for tests and hosts without a database.
#[derive(Default)]
pub struct MemoryKv(Mutex<HashMap<String, Value>>);

impl Kv for MemoryKv {
    fn get(&self, key: &str) -> anyhow::Result<Option<Value>> {
        Ok(self.0.lock().get(key).cloned())
    }
    fn set(&self, key: &str, value: &Value) -> anyhow::Result<()> {
        self.0.lock().insert(key.to_string(), value.clone());
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Status {
    Cached,
    Cloned,
    Refreshed,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EnsureResult {
    pub repository: String,
    pub host: String,
    pub remote: String,
    pub local_path: String,
    pub status: Status,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub head: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub branch: Option<String>,
}

/// `refresh?: boolean | "daily"`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Refresh {
    #[default]
    Never,
    Always,
    /// Throttle existing checkouts to one refresh attempt per day.
    Daily,
}

#[derive(Debug, Clone)]
pub struct EnsureInput {
    pub reference: Reference,
    pub refresh: Refresh,
    pub branch: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum CacheError {
    #[error("{message}")]
    InvalidBranch { branch: String, message: String },
    #[error("{message}")]
    CloneFailed { repository: String, message: String },
    #[error("{message}")]
    FetchFailed { repository: String, message: String },
    #[error("{message}")]
    CheckoutFailed { repository: String, branch: String, message: String },
    #[error("{message}")]
    ResetFailed { repository: String, message: String },
    #[error("{message}")]
    LockFailed { local_path: String, message: String },
    #[error("{message}")]
    CacheOperation { operation: String, path: String, message: String },
}

impl CacheError {
    pub fn tag(&self) -> &'static str {
        match self {
            CacheError::InvalidBranch { .. } => "RepositoryCacheInvalidBranchError",
            CacheError::CloneFailed { .. } => "RepositoryCacheCloneFailedError",
            CacheError::FetchFailed { .. } => "RepositoryCacheFetchFailedError",
            CacheError::CheckoutFailed { .. } => "RepositoryCacheCheckoutFailedError",
            CacheError::ResetFailed { .. } => "RepositoryCacheResetFailedError",
            CacheError::LockFailed { .. } => "RepositoryCacheLockFailedError",
            CacheError::CacheOperation { .. } => "RepositoryCacheOperationError",
        }
    }
}

pub fn validate_branch(branch: &str) -> Result<(), CacheError> {
    repository::validate_branch(branch).map_err(|error| match error {
        RepositoryError::InvalidBranch { branch, message } => CacheError::InvalidBranch { branch, message },
        other => CacheError::InvalidBranch { branch: branch.to_string(), message: other.to_string() },
    })
}

pub struct RepositoryCache {
    git: Git,
    /// `Global.repos` (`<data>/repos`).
    repos: PathBuf,
    /// Lock root (`<state>/locks`).
    locks: PathBuf,
    kv: std::sync::Arc<dyn Kv>,
}

fn cache_operation(operation: &str, target: &Path, error: impl ToString) -> CacheError {
    CacheError::CacheOperation {
        operation: operation.to_string(),
        path: target.to_string_lossy().into_owned(),
        message: error.to_string(),
    }
}

fn now_ms() -> f64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis() as f64).unwrap_or(0.0)
}

impl RepositoryCache {
    pub fn new(git: Git, repos: PathBuf, locks: PathBuf, kv: std::sync::Arc<dyn Kv>) -> Self {
        RepositoryCache { git, repos, locks, kv }
    }

    pub async fn ensure(&self, input: EnsureInput) -> Result<EnsureResult, CacheError> {
        if let Some(branch) = &input.branch {
            validate_branch(branch)?;
        }
        let branch_input = input.branch.as_deref().filter(|b| !b.is_empty());
        let label = input.reference.label.clone();
        let local_path = repository::cache_path(&self.repos, &input.reference, branch_input);
        let local = local_path.to_string_lossy().into_owned();
        let key = format!("repository-cache:{local}");
        let clone_target = repository::parse(&input.reference.remote).unwrap_or_else(|| input.reference.clone());

        let lease = oc_util::flock::acquire(&self.locks, &key, oc_util::flock::Options::default())
            .await
            .map_err(|error| CacheError::LockFailed { local_path: local.clone(), message: error.to_string() })?;
        let result = self.locked(&input, branch_input, &label, &local_path, &key, &clone_target).await;
        drop(lease);
        result
    }

    async fn locked(
        &self,
        input: &EnsureInput,
        branch_input: Option<&str>,
        label: &str,
        local_path: &Path,
        key: &str,
        clone_target: &Reference,
    ) -> Result<EnsureResult, CacheError> {
        if let Some(parent) = local_path.parent() {
            tokio::fs::create_dir_all(parent)
                .await
                .map_err(|e| cache_operation("ensure cache directory", local_path, e))?;
        }
        let existing = self.git.discover(local_path).await;
        let origin = match &existing {
            Some(existing) => self.git.remote(existing, None).await,
            None => None,
        };
        let origin_reference = origin.as_deref().and_then(repository::parse);
        // Discovery walks upward, so reuse requires the checkout to live exactly at the cache path.
        let worktree = existing.as_ref().map(|_| paths::real_or_resolved(local_path));
        let reuse = match (&existing, &origin_reference) {
            (Some(existing), Some(origin)) => {
                Some(&existing.worktree) == worktree.as_ref() && repository::same(origin, clone_target)
            }
            _ => false,
        };
        if !reuse && tokio::fs::try_exists(local_path).await.unwrap_or(false) {
            let removed = match tokio::fs::symlink_metadata(local_path).await {
                Ok(meta) if meta.is_dir() => tokio::fs::remove_dir_all(local_path).await,
                _ => tokio::fs::remove_file(local_path).await,
            };
            removed.map_err(|e| cache_operation("remove stale cache", local_path, e))?;
        }

        let now = now_ms();
        let previous = self
            .kv
            .get(key)
            .ok()
            .flatten()
            .and_then(|value| value.get("attemptedAt").and_then(Value::as_f64));
        let refresh = match input.refresh {
            Refresh::Daily => previous.is_none_or(|attempted| now - attempted >= REFRESH_INTERVAL_MS),
            Refresh::Always => true,
            Refresh::Never => false,
        };
        let status = if !reuse {
            Status::Cloned
        } else if refresh {
            Status::Refreshed
        } else {
            Status::Cached
        };
        let repository = label.to_string();

        if status != Status::Cached {
            // Record attempts before network work so failures obey the same refresh interval.
            self.kv
                .set(key, &json!({ "attemptedAt": now as u64 }))
                .map_err(|e| cache_operation("record refresh attempt", local_path, e))?;

            if status == Status::Cloned {
                self.git
                    .clone_repository(CloneInput {
                        remote: input.reference.remote.clone(),
                        directory: local_path.to_path_buf(),
                        branch: branch_input.map(str::to_string),
                        depth: None,
                    })
                    .await
                    .map_err(|e| CacheError::CloneFailed { repository: repository.clone(), message: e.message })?;
            }

            if status == Status::Refreshed {
                let Some(existing) = &existing else {
                    return Err(CacheError::FetchFailed {
                        repository,
                        message: "Repository is unavailable".to_string(),
                    });
                };
                self.git
                    .fetch_remotes(existing, None)
                    .await
                    .map_err(|e| CacheError::FetchFailed { repository: repository.clone(), message: e.message })?;
                if let Some(branch) = branch_input {
                    self.git
                        .fetch_branch(existing, FetchBranchInput { remote: None, branch: branch.to_string(), force: None })
                        .await
                        .map_err(|e| CacheError::FetchFailed { repository: repository.clone(), message: e.message })?;
                }
                // Checking out the tracked ref before resetting keeps the checkout self-healing.
                let branch = match branch_input {
                    Some(branch) => Some(branch.to_string()),
                    None => self.git.default_remote_branch(existing, None).await,
                };
                if let Some(branch) = &branch {
                    self.git
                        .checkout_remote_branch(
                            existing,
                            CheckoutInput { remote: None, branch: branch.clone(), reset: None },
                        )
                        .await
                        .map_err(|e| CacheError::CheckoutFailed {
                            repository: repository.clone(),
                            branch: branch.clone(),
                            message: e.message,
                        })?;
                }
                let target = match branch {
                    Some(branch) => Some(branch),
                    None => self.git.branch(existing).await,
                };
                let revision = target.map(|t| format!("origin/{t}")).unwrap_or_else(|| "HEAD".to_string());
                self.git
                    .reset_hard(existing, &revision)
                    .await
                    .map_err(|e| CacheError::ResetFailed { repository: repository.clone(), message: e.message })?;
            }
        }

        let checkout = self.git.discover(local_path).await;
        let (head, branch) = match &checkout {
            Some(checkout) => (self.git.head(checkout).await, self.git.branch(checkout).await),
            None => (None, None),
        };
        Ok(EnsureResult {
            repository,
            host: input.reference.host.clone(),
            remote: input.reference.remote.clone(),
            local_path: local_path.to_string_lossy().into_owned(),
            status,
            head,
            branch,
        })
    }
}
