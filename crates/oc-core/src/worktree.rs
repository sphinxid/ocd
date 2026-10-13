//! Worktrees (`core/src/worktree.ts`, `worktree/git.ts` via oc-git) and the strategy registry
//! (`core/src/worktree/strategies.ts`) plugins extend through `worktree` transforms
//! ([`crate::plugins::Plugins::worktree_strategies`]).

use std::path::{Path, PathBuf};
use std::sync::Arc;

use async_trait::async_trait;

use rusqlite::{params, OptionalExtension};
use serde_json::{json, Value};

use crate::bus::Bus;

#[derive(Debug, thiserror::Error)]
pub enum WorktreeError {
    #[error("Project not found: {0}")]
    ProjectNotFound(String),
    #[error("Source directory not found: {0}")]
    SourceDirectoryNotFound(String),
    #[error("Worktree destination already exists: {0}")]
    DestinationExists(String),
    #[error("Invalid worktree directory: {0}")]
    InvalidDirectory(String),
    /// `Worktree.OperationError` (message as upstream's `operationError` builds it).
    #[error("{0}")]
    Operation(String),
    #[error("Worktree directory unavailable: {0}")]
    DirectoryUnavailable(String),
    #[error("Worktree strategy unavailable: {0}")]
    StrategyUnavailable(String),
    #[error("{message}")]
    Git { message: String, force_required: Option<bool> },
    #[error("{0}")]
    StartCommand(String),
}

/// A strategy failure: a git/operation error carried as is (`Git.WorktreeError`,
/// `Worktree.OperationError`), or anything else, which `operationError` wraps.
#[derive(Debug, Clone, PartialEq)]
pub enum StrategyError {
    Worktree { message: String, force_required: Option<bool> },
    Other(String),
}

impl StrategyError {
    fn into_error(self, strategy: &str, operation: &str) -> WorktreeError {
        match self {
            StrategyError::Worktree { message, force_required } => WorktreeError::Git { message, force_required },
            StrategyError::Other(message) => WorktreeError::Operation(format!("Worktree strategy {strategy} failed to {operation}: {message}")),
        }
    }
}

/// `WorktreeStrategies.Strategy`.
#[async_trait]
pub trait Strategy: Send + Sync {
    fn id(&self) -> &str;
    /// `create({sourceDirectory, directory, branch?})` → the created directory.
    async fn create(&self, directory: &Path, source: &Path, branch: Option<&str>) -> Result<PathBuf, StrategyError>;
    async fn remove(&self, directory: &Path, force: bool) -> Result<(), StrategyError>;
    async fn list(&self, root: &Path) -> Result<Vec<oc_git::ListEntry>, StrategyError>;
}

/// The built-in git strategy (`core/src/worktree/git.ts`).
pub struct GitStrategy;

#[async_trait]
impl Strategy for GitStrategy {
    fn id(&self) -> &str {
        oc_git::GitWorktreeStrategy::ID
    }
    async fn create(&self, directory: &Path, source: &Path, branch: Option<&str>) -> Result<PathBuf, StrategyError> {
        oc_git::GitWorktreeStrategy::new(oc_git::Git::new()).create(directory, source, branch).await.map_err(|e| StrategyError::Other(e.to_string()))
    }
    async fn remove(&self, directory: &Path, force: bool) -> Result<(), StrategyError> {
        oc_git::GitWorktreeStrategy::new(oc_git::Git::new()).remove(directory, force).await.map_err(|e| {
            let force_required = match &e {
                oc_git::WorktreeStrategyError::Git(w) => w.force_required,
                _ => None,
            };
            StrategyError::Worktree { message: e.to_string(), force_required }
        })
    }
    async fn list(&self, root: &Path) -> Result<Vec<oc_git::ListEntry>, StrategyError> {
        oc_git::GitWorktreeStrategy::new(oc_git::Git::new()).list(root).await.map_err(|e| StrategyError::Other(e.to_string()))
    }
}

/// A location's strategies in registration order and the selected one (`get()`).
#[derive(Clone)]
pub struct Strategies {
    pub strategies: Vec<Arc<dyn Strategy>>,
    pub selected: String,
}

impl Default for Strategies {
    /// The initial state: the git strategy, selected.
    fn default() -> Self {
        Strategies { strategies: vec![Arc::new(GitStrategy)], selected: oc_git::GitWorktreeStrategy::ID.into() }
    }
}

impl Strategies {
    pub fn get(&self, id: &str) -> Result<Arc<dyn Strategy>, WorktreeError> {
        self.strategies.iter().find(|s| s.id() == id).cloned().ok_or_else(|| WorktreeError::StrategyUnavailable(id.into()))
    }
}

/// The strategies of project `project_id`, which upstream reads from the project's canonical
/// checkout location (`Worktree.load`): that location's plugin `worktree` transforms, after its
/// activation. The built-in git strategy when the project is unknown or the location has no plugins.
pub async fn strategies(plugins: &crate::plugins::Plugins, bus: &Bus, project_id: &str) -> Strategies {
    let Some((worktree, _)) = project_row(bus, project_id).ok().flatten() else { return Strategies::default() };
    plugins.await_activation(&worktree).await;
    plugins.worktree_strategies(&worktree).await
}

/// The project's canonical checkout directory.
pub fn project_worktree(bus: &Bus, project_id: &str) -> Option<String> {
    project_row(bus, project_id).ok().flatten().map(|(w, _)| w)
}

fn project_row(bus: &Bus, project_id: &str) -> anyhow::Result<Option<(String, Option<String>)>> {
    bus.db().with(|c| {
        c.query_row("SELECT worktree, commands FROM project WHERE id = ?", [project_id], |r| Ok((r.get(0)?, r.get(1)?)))
            .optional()
    })
}

/// Parent directory for new worktrees (`worktree.directory` config or `<data>/worktree/<id6>`).
pub fn parent_directory(global: &oc_util::Global, project_id: &str, canonical: &Path, configured: Option<&str>) -> PathBuf {
    match configured {
        Some(d) if d.starts_with("~/") => global.home.join(&d[2..]),
        Some(d) => crate::session::relative_join(canonical, d),
        None => oc_git::worktree::default_directory(&global.data, project_id),
    }
}

pub async fn create(
    bus: &Bus,
    global: &oc_util::Global,
    project_id: &str,
    from: Option<&str>,
    branch: Option<&str>,
    directory: Option<&Path>,
    name: Option<&str>,
    configured_parent: Option<&str>,
    strategies: &Strategies,
) -> Result<PathBuf, WorktreeError> {
    let (worktree, commands) =
        project_row(bus, project_id).ok().flatten().ok_or_else(|| WorktreeError::ProjectNotFound(project_id.into()))?;
    let selected = strategies.get(&strategies.selected)?;
    let parent = directory.map(Path::to_path_buf).unwrap_or_else(|| parent_directory(global, project_id, Path::new(&worktree), configured_parent));
    let source = std::fs::canonicalize(from.unwrap_or(&worktree)).unwrap_or_else(|_| PathBuf::from(from.unwrap_or(&worktree)));
    let known: bool = bus
        .db()
        .with(|c| {
            c.query_row(
                "SELECT 1 FROM worktree WHERE project_id = ? AND directory = ?",
                params![project_id, source.display().to_string()],
                |_| Ok(()),
            )
            .optional()
        })
        .ok()
        .flatten()
        .is_some();
    if !known {
        return Err(WorktreeError::SourceDirectoryNotFound(source.display().to_string()));
    }
    std::fs::create_dir_all(&parent).map_err(|e| WorktreeError::Operation(e.to_string()))?;
    let target = oc_git::worktree::allocate_directory(&parent, name)
        .map_err(|_| WorktreeError::DestinationExists(parent.join(name.unwrap_or("")).display().to_string()))?;
    let created = selected.create(&target, &source, branch).await.map_err(|e| e.into_error(selected.id(), "create"))?;
    let created = oc_git::worktree::canonical(&created).map_err(|_| WorktreeError::DirectoryUnavailable(created.display().to_string()))?;
    let dir = created.display().to_string();
    let changed = bus
        .db()
        .with(|c| {
            c.execute(
                "INSERT INTO worktree (project_id, directory, strategy, time_created) VALUES (?, ?, ?, ?) ON CONFLICT(project_id, directory) DO UPDATE SET strategy = excluded.strategy",
                params![project_id, dir, selected.id(), oc_schema::id::now_ms() as i64],
            )
        })
        .unwrap_or(0);
    if changed > 0 {
        bus.publish("worktree.updated", json!({"projectID": project_id}), None);
    }
    let start = commands
        .and_then(|c| serde_json::from_str::<Value>(&c).ok())
        .and_then(|v| v["start"].as_str().map(|s| s.trim().to_string()))
        .filter(|s| !s.is_empty());
    if let Some(start) = start {
        let status = tokio::process::Command::new("bash")
            .args(["-lc", &start])
            .current_dir(&created)
            .env("OPENCODE_WORKTREE_BASE", &source)
            .env("OPENCODE_WORKTREE_PATH", &created)
            .stdin(std::process::Stdio::null())
            .status()
            .await
            .map_err(|e| WorktreeError::StartCommand(e.to_string()))?;
        if !status.success() {
            return Err(WorktreeError::StartCommand(format!("Worktree start command failed with exit code {}", status.code().unwrap_or(-1))));
        }
    }
    Ok(created)
}

pub async fn remove(bus: &Bus, project_id: &str, directory: &str, force: bool, strategies: &Strategies) -> Result<(), WorktreeError> {
    let dir = std::fs::canonicalize(directory).map(|p| p.display().to_string()).unwrap_or_else(|_| directory.to_string());
    let strategy: Option<Option<String>> = bus
        .db()
        .with(|c| {
            c.query_row("SELECT strategy FROM worktree WHERE project_id = ? AND directory = ?", params![project_id, dir], |r| r.get(0))
                .optional()
        })
        .ok()
        .flatten();
    let Some(Some(stored)) = strategy else {
        return Err(WorktreeError::InvalidDirectory(dir));
    };
    // The strategy that created it, not the selected one.
    let strategy = strategies.get(&stored)?;
    strategy.remove(Path::new(&dir), force).await.map_err(|e| e.into_error(strategy.id(), "remove"))?;
    let removed = bus
        .db()
        .with(|c| c.execute("DELETE FROM worktree WHERE project_id = ? AND directory = ?", params![project_id, dir]))
        .unwrap_or(0);
    if removed > 0 {
        bus.publish("worktree.updated", json!({"projectID": project_id}), None);
    }
    Ok(())
}

/// `Worktree.refresh`: discover worktrees under the project root and every unowned (checkout) row, claim
/// unowned rows for the strategy that manages them, and drop rows whose directory no longer exists.
pub async fn refresh(bus: &Bus, project_id: &str, strategies: &Strategies) -> Result<(), WorktreeError> {
    let (worktree, _) =
        project_row(bus, project_id).ok().flatten().ok_or_else(|| WorktreeError::ProjectNotFound(project_id.into()))?;
    if !Path::new(&worktree).is_dir() {
        return Err(WorktreeError::DirectoryUnavailable(worktree));
    }
    let stored: Vec<(String, Option<String>, bool)> = list(bus, project_id)
        .unwrap_or_default()
        .into_iter()
        .map(|v| {
            let dir = v["directory"].as_str().unwrap_or_default().to_string();
            let exists = Path::new(&dir).is_dir();
            (dir, v["strategy"].as_str().map(str::to_string), exists)
        })
        .collect();
    // Unowned rows are checkout/discovery roots. Managed children are enumerated by their backend.
    let mut roots = vec![worktree.clone()];
    for (dir, strategy, exists) in &stored {
        if *exists && strategy.is_none() && !roots.contains(dir) {
            roots.push(dir.clone());
        }
    }
    // Newest registration first: the first strategy to report a directory owns it.
    let mut discovered: Vec<(String, Option<String>)> = Vec::new();
    for root in &roots {
        if !Path::new(root).is_dir() {
            continue;
        }
        for strategy in strategies.strategies.iter().rev() {
            let entries = match strategy.list(Path::new(root)).await {
                Ok(entries) => entries,
                Err(error) => {
                    let error = error.into_error(strategy.id(), "list");
                    tracing::warn!(directory = %root, strategy = strategy.id(), error = %error, "worktree discovery failed");
                    continue;
                }
            };
            for entry in entries {
                let Ok(dir) = oc_git::worktree::canonical(&entry.directory) else { continue };
                let dir = dir.display().to_string();
                if discovered.iter().any(|(d, _)| *d == dir) {
                    continue;
                }
                let owner = (entry.kind == oc_git::worktree::ListEntryType::Worktree).then(|| strategy.id().to_string());
                discovered.push((dir, owner));
            }
        }
    }
    let missing: Vec<String> = stored.iter().filter(|(_, _, exists)| !exists).map(|(d, _, _)| d.clone()).collect();
    let now = oc_schema::id::now_ms() as i64;
    let changed = bus
        .db()
        .with(|c| {
            let tx = c.unchecked_transaction()?;
            let mut changed = false;
            for (dir, owner) in &discovered {
                // Discovery may claim an unowned row, but never replace another strategy's ownership.
                let n = match owner {
                    Some(owner) => tx.execute(
                        "INSERT INTO worktree (project_id, directory, strategy, time_created) VALUES (?, ?, ?, ?) ON CONFLICT(project_id, directory) DO UPDATE SET strategy = excluded.strategy WHERE worktree.strategy IS NULL",
                        params![project_id, dir, owner, now],
                    )?,
                    None => tx.execute(
                        "INSERT INTO worktree (project_id, directory, strategy, time_created) VALUES (?, ?, NULL, ?) ON CONFLICT(project_id, directory) DO NOTHING",
                        params![project_id, dir, now],
                    )?,
                };
                changed |= n > 0;
            }
            for dir in &missing {
                changed |= tx.execute("DELETE FROM worktree WHERE project_id = ? AND directory = ?", params![project_id, dir])? > 0;
            }
            tx.commit()?;
            Ok(changed)
        })
        .map_err(|e| WorktreeError::Operation(e.to_string()))?;
    if changed {
        bus.publish("worktree.updated", json!({"projectID": project_id}), None);
    }
    Ok(())
}

/// `[{directory, strategy?}]` ordered by `time_created DESC, directory ASC`.
pub fn list(bus: &Bus, project_id: &str) -> anyhow::Result<Vec<Value>> {
    bus.db().with(|c| {
        let mut s = c.prepare("SELECT directory, strategy FROM worktree WHERE project_id = ? ORDER BY time_created DESC, directory ASC")?;
        let r = s
            .query_map([project_id], |r| {
                let dir: String = r.get(0)?;
                let strategy: Option<String> = r.get(1)?;
                let mut v = json!({"directory": dir});
                if let Some(st) = strategy {
                    v["strategy"] = json!(st);
                }
                Ok(v)
            })?
            .collect();
        r
    })
}
