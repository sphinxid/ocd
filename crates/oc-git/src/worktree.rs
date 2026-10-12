//! Git parts of `packages/core/src/worktree.ts` and `worktree/{git,directory,strategies}.ts`, plus
//! `util/slug.ts`. Database bookkeeping (the `worktree` table, events, start commands) lives in oc-core.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::git::{Git, WorktreeError, WorktreeKind};
use crate::paths;

pub const ADJECTIVES: &[&str] = &[
    "brave", "calm", "clever", "cosmic", "crisp", "curious", "eager", "gentle", "glowing", "happy", "hidden", "jolly",
    "kind", "lucky", "mighty", "misty", "neon", "nimble", "playful", "proud", "quick", "quiet", "shiny", "silent",
    "stellar", "sunny", "swift", "tidy", "witty",
];

pub const NOUNS: &[&str] = &[
    "cabin", "cactus", "canyon", "circuit", "comet", "eagle", "engine", "falcon", "forest", "garden", "harbor",
    "island", "knight", "lagoon", "meadow", "moon", "mountain", "nebula", "orchid", "otter", "panda", "pixel",
    "planet", "river", "rocket", "sailor", "squid", "star", "tiger", "wizard", "wolf",
];

/// `Slug.create()`: `<adjective>-<noun>`.
pub fn slug() -> String {
    format!(
        "{}-{}",
        ADJECTIVES[rand::random_range(0..ADJECTIVES.len())],
        NOUNS[rand::random_range(0..NOUNS.len())]
    )
}

/// Default parent directory for new worktrees: `<data>/worktree/<projectID[0..6]>`.
pub fn default_directory(data: &Path, project_id: &str) -> PathBuf {
    let prefix: String = project_id.chars().take(6).collect();
    data.join("worktree").join(prefix)
}

#[derive(Debug, Clone, thiserror::Error)]
pub enum WorktreeStrategyError {
    /// `Worktree.DirectoryUnavailableError`
    #[error("Directory unavailable: {}", directory.display())]
    DirectoryUnavailable { directory: PathBuf },
    /// `Worktree.DestinationExistsError`
    #[error("Destination exists: {}", directory.display())]
    DestinationExists { directory: PathBuf },
    /// `Git.WorktreeError`
    #[error(transparent)]
    Git(#[from] WorktreeError),
}

impl WorktreeStrategyError {
    pub fn tag(&self) -> &'static str {
        match self {
            WorktreeStrategyError::DirectoryUnavailable { .. } => "Worktree.DirectoryUnavailableError",
            WorktreeStrategyError::DestinationExists { .. } => "Worktree.DestinationExistsError",
            WorktreeStrategyError::Git(_) => WorktreeError::TAG,
        }
    }
}

/// `canonical(fs, input)`: realpath (or resolved path) that must be an existing directory.
pub fn canonical(input: &Path) -> Result<PathBuf, WorktreeStrategyError> {
    let resolved = paths::real_or_resolved(input);
    if !resolved.is_dir() {
        return Err(WorktreeStrategyError::DirectoryUnavailable { directory: input.to_path_buf() });
    }
    Ok(resolved)
}

/// Pick `<parent>/<name>`, then `<name>-2` … `<name>-10`; creates `parent` first (`mkdir -p`).
pub fn allocate_directory(parent: &Path, name: Option<&str>) -> Result<PathBuf, WorktreeStrategyError> {
    std::fs::create_dir_all(parent)
        .map_err(|_| WorktreeStrategyError::DirectoryUnavailable { directory: parent.to_path_buf() })?;
    let name = name.map(str::to_string).unwrap_or_else(slug);
    let mut suffix = 1;
    let mut directory = parent.join(&name);
    while directory.exists() {
        suffix += 1;
        if suffix > 10 {
            return Err(WorktreeStrategyError::DestinationExists { directory });
        }
        directory = parent.join(format!("{name}-{suffix}"));
    }
    Ok(directory)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ListEntryType {
    Root,
    Worktree,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ListEntry {
    pub directory: PathBuf,
    #[serde(rename = "type")]
    pub kind: ListEntryType,
}

/// The `git` worktree strategy (`worktree/git.ts`).
#[derive(Clone, Default)]
pub struct GitWorktreeStrategy {
    git: Git,
}

impl GitWorktreeStrategy {
    pub const ID: &'static str = "git";

    pub fn new(git: Git) -> Self {
        GitWorktreeStrategy { git }
    }

    /// Discover the source repo, `git worktree add --detach -- <dir> <branch or HEAD>`, realpath the result.
    pub async fn create(
        &self,
        directory: &Path,
        source_directory: &Path,
        branch: Option<&str>,
    ) -> Result<PathBuf, WorktreeStrategyError> {
        let Some(repository) = self.git.discover(source_directory).await else {
            return Err(WorktreeStrategyError::DirectoryUnavailable { directory: source_directory.to_path_buf() });
        };
        self.git.worktree_create(&repository, directory, branch).await?;
        canonical(directory)
    }

    /// Discover the repository at `directory`, then `git worktree remove [--force] <dir>`.
    pub async fn remove(&self, directory: &Path, force: bool) -> Result<(), WorktreeStrategyError> {
        let Some(repository) = self.git.discover(directory).await else {
            return Err(WorktreeStrategyError::DirectoryUnavailable { directory: directory.to_path_buf() });
        };
        self.git.worktree_remove(&repository, directory, force).await?;
        Ok(())
    }

    /// `git worktree list --porcelain`: main → `root`, linked → `worktree`; missing directories skipped.
    pub async fn list(&self, directory: &Path) -> Result<Vec<ListEntry>, WorktreeStrategyError> {
        let Some(repository) = self.git.discover(directory).await else {
            return Err(WorktreeStrategyError::DirectoryUnavailable { directory: directory.to_path_buf() });
        };
        let entries = self.git.worktree_list(&repository).await?;
        Ok(entries
            .into_iter()
            .filter_map(|entry| {
                canonical(&entry.directory).ok().map(|directory| ListEntry {
                    directory,
                    kind: if entry.kind == WorktreeKind::Main { ListEntryType::Root } else { ListEntryType::Worktree },
                })
            })
            .collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slug_shape() {
        for _ in 0..50 {
            let value = slug();
            let (adjective, noun) = value.split_once('-').unwrap();
            assert!(ADJECTIVES.contains(&adjective));
            assert!(NOUNS.contains(&noun));
        }
        assert_eq!(ADJECTIVES.len(), 29);
        assert_eq!(NOUNS.len(), 31);
    }

    #[test]
    fn default_directory_uses_six_chars() {
        assert_eq!(
            default_directory(Path::new("/data/opencode"), "abcdef0123"),
            PathBuf::from("/data/opencode/worktree/abcdef")
        );
    }

    #[test]
    fn allocates_with_suffixes() {
        let root = tempfile::tempdir().unwrap();
        let parent = root.path().join("parent");
        let first = allocate_directory(&parent, Some("calm-moon")).unwrap();
        assert_eq!(first, parent.join("calm-moon"));
        std::fs::create_dir(&first).unwrap();
        let second = allocate_directory(&parent, Some("calm-moon")).unwrap();
        assert_eq!(second, parent.join("calm-moon-2"));
        for n in 2..=10 {
            std::fs::create_dir_all(parent.join(format!("calm-moon-{n}"))).unwrap();
        }
        assert!(matches!(
            allocate_directory(&parent, Some("calm-moon")),
            Err(WorktreeStrategyError::DestinationExists { .. })
        ));
    }
}
