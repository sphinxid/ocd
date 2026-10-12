//! Worktrees (`core/src/worktree.ts`, `worktree/git.ts` via oc-git).

use std::path::{Path, PathBuf};

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
    #[error("Worktree strategy git failed to create: {0}")]
    Operation(String),
    #[error("{message}")]
    Git { message: String, force_required: bool },
    #[error("{0}")]
    StartCommand(String),
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
) -> Result<PathBuf, WorktreeError> {
    let (worktree, commands) =
        project_row(bus, project_id).ok().flatten().ok_or_else(|| WorktreeError::ProjectNotFound(project_id.into()))?;
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
    let created = oc_git::GitWorktreeStrategy::new(oc_git::Git::new())
        .create(&target, &source, branch)
        .await
        .map_err(|e| WorktreeError::Operation(e.to_string()))?;
    let dir = created.display().to_string();
    let changed = bus
        .db()
        .with(|c| {
            c.execute(
                "INSERT INTO worktree (project_id, directory, strategy, time_created) VALUES (?, ?, 'git', ?) ON CONFLICT(project_id, directory) DO UPDATE SET strategy = 'git'",
                params![project_id, dir, oc_schema::id::now_ms() as i64],
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

pub async fn remove(bus: &Bus, project_id: &str, directory: &str, force: bool) -> Result<(), WorktreeError> {
    let dir = std::fs::canonicalize(directory).map(|p| p.display().to_string()).unwrap_or_else(|_| directory.to_string());
    let strategy: Option<Option<String>> = bus
        .db()
        .with(|c| {
            c.query_row("SELECT strategy FROM worktree WHERE project_id = ? AND directory = ?", params![project_id, dir], |r| r.get(0))
                .optional()
        })
        .ok()
        .flatten();
    if !matches!(strategy, Some(Some(_))) {
        return Err(WorktreeError::InvalidDirectory(dir));
    }
    oc_git::GitWorktreeStrategy::new(oc_git::Git::new()).remove(Path::new(&dir), force).await.map_err(|e| {
        let force_required = matches!(&e, oc_git::WorktreeStrategyError::Git(w) if w.force_required == Some(true));
        WorktreeError::Git { message: e.to_string(), force_required }
    })?;
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
pub async fn refresh(bus: &Bus, project_id: &str) -> Result<(), WorktreeError> {
    let (worktree, _) =
        project_row(bus, project_id).ok().flatten().ok_or_else(|| WorktreeError::ProjectNotFound(project_id.into()))?;
    if !Path::new(&worktree).is_dir() {
        return Err(WorktreeError::Operation(format!("Directory unavailable: {worktree}")));
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
    let strategy = oc_git::GitWorktreeStrategy::new(oc_git::Git::new());
    let mut discovered: Vec<(String, Option<&'static str>)> = Vec::new();
    for root in &roots {
        if !Path::new(root).is_dir() {
            continue;
        }
        let entries = match strategy.list(Path::new(root)).await {
            Ok(entries) => entries,
            Err(error) => {
                tracing::warn!(directory = %root, strategy = oc_git::GitWorktreeStrategy::ID, error = %error, "worktree discovery failed");
                continue;
            }
        };
        for entry in entries {
            let Ok(dir) = oc_git::worktree::canonical(&entry.directory) else { continue };
            let dir = dir.display().to_string();
            if discovered.iter().any(|(d, _)| *d == dir) {
                continue;
            }
            let owner = (entry.kind == oc_git::worktree::ListEntryType::Worktree).then_some(oc_git::GitWorktreeStrategy::ID);
            discovered.push((dir, owner));
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
