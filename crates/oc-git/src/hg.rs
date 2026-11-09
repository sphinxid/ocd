//! Port of the Mercurial VCS provider `packages/core/src/plugin/vcs/hg.ts` (`opencode.vcs.hg`).
//!
//! [`HgVcs`] is the adapter oc-core's `Vcs` service calls: `info`, `branches`, `status`, `diff`
//! and `init`. `hg diff --git` emits git-format patches for tracked changes; untracked (`?`) and
//! missing (`!`) files never appear in `hg diff`, so their patches are synthesized from file
//! contents. Every command runs `hg` from `PATH` with `HGPLAIN=1`, from the worktree root (hg
//! prints root-relative paths), scoped to the Location directory by a relative pathspec.

use std::path::{Path, PathBuf};

use oc_schema::types::{FileDiff, FileStatus as Kind};

use crate::patch::{self, MAX_PATCH_BYTES, MAX_TOTAL_PATCH_BYTES, PATCH_CONTEXT_LINES, Patch};
use crate::paths;
use crate::process::{self, RunOptions};
use crate::vcs::{Branch, DiffError, DiffOptions, Info, Mode, VcsFileStatus, locale_compare};

/// Failure message of `init`.
pub const INIT_FAILED_MESSAGE: &str = "Mercurial initialization failed";
/// The `DiffError` of committed reviews and explicit bases.
pub const UNSUPPORTED_REVIEW_MESSAGE: &str = "The Mercurial provider does not support committed reviews or explicit bases";

struct Out {
    exit_code: i32,
    text: String,
    truncated: bool,
}

/// Runs `hg <args>` in `cwd` with `HGPLAIN=1`; spawn failures read as exit code 1 with no output.
async fn run(cwd: &Path, args: Vec<String>, max: Option<usize>) -> Out {
    let options = RunOptions { env: vec![("HGPLAIN", "1".to_string())], stdin: None, max_output_bytes: max };
    match process::run_program("hg", cwd, &args, options).await {
        Ok(result) => Out { exit_code: result.exit_code, text: result.text(), truncated: result.stdout_truncated },
        Err(_) => Out { exit_code: 1, text: String::new(), truncated: false },
    }
}

fn kind(code: &str) -> Kind {
    match code {
        "A" | "?" => Kind::Added,
        "R" | "!" => Kind::Deleted,
        _ => Kind::Modified,
    }
}

#[derive(Debug, Clone)]
struct Item {
    file: String,
    code: String,
    status: Kind,
}

/// The Mercurial adapter of one Location.
#[derive(Debug, Clone)]
pub struct HgVcs {
    worktree: PathBuf,
    /// The Location directory relative to the worktree, or `.`.
    scope: String,
}

impl HgVcs {
    pub const ID: &'static str = "hg";
    pub const NAME: &'static str = "Mercurial";

    pub fn new(directory: impl Into<PathBuf>, worktree: impl Into<PathBuf>) -> Self {
        let directory = directory.into();
        let worktree = worktree.into();
        let relative = paths::relative(&worktree, &directory);
        let scope = if relative.is_empty() { ".".to_string() } else { relative };
        HgVcs { worktree, scope }
    }

    /// `hg init` in the worktree.
    pub async fn init(worktree: &Path) -> Result<(), DiffError> {
        let out = run(worktree, vec!["init".into()], None).await;
        if out.exit_code != 0 {
            return Err(DiffError { message: INIT_FAILED_MESSAGE.into() });
        }
        Ok(())
    }

    pub async fn info(&self) -> Info {
        Info { provider: None, branch: Branch { current: self.branch().await, default: Some("default".into()) } }
    }

    /// Mercurial lists no branches.
    pub async fn branches(&self) -> Vec<String> {
        vec![]
    }

    pub async fn status(&self) -> Vec<VcsFileStatus> {
        self.diff_against(None, 0)
            .await
            .into_iter()
            .map(|item| VcsFileStatus { file: item.file, additions: item.additions, deletions: item.deletions, status: item.status })
            .collect()
    }

    /// `working` diffs the working copy; `branch` diffs a named branch against its common ancestor
    /// with `default` (nothing on `default` itself); `committed` and explicit bases are refused.
    pub async fn diff(&self, mode: Mode, options: &DiffOptions) -> Result<Vec<FileDiff>, DiffError> {
        let context = options.context.unwrap_or(PATCH_CONTEXT_LINES);
        if mode == Mode::Working {
            return Ok(self.diff_against(None, context).await);
        }
        if mode == Mode::Committed || options.base.is_some() {
            return Err(DiffError { message: UNSUPPORTED_REVIEW_MESSAGE.into() });
        }
        if self.branch().await.is_none_or(|branch| branch == "default") {
            return Ok(vec![]);
        }
        let Some(ancestor) = self.ancestor().await else { return Ok(vec![]) };
        Ok(self.diff_against(Some(&ancestor), context).await)
    }

    async fn diff_against(&self, rev: Option<&str>, context: u64) -> Vec<FileDiff> {
        let (mut items, batch) = tokio::join!(self.status_names(rev), self.diff_patch(rev, context));
        let chunks = patch::chunks_by_file(&batch, |_| None);
        items.sort_by(|a, b| locale_compare(&a.file, &b.file));
        let mut list = Vec::with_capacity(items.len());
        for item in items {
            let patch = match chunks.get(&item.file) {
                Some(chunk) => chunk.clone(),
                None => self.synthesized(&item, rev).await,
            };
            let (additions, deletions) = patch::count_patch(&patch);
            list.push(FileDiff { file: item.file, patch, additions, deletions, status: item.status, more: None });
        }
        list
    }

    /// `patchFor` without a chunk: untracked files from their content, missing ones from `rev`.
    async fn synthesized(&self, item: &Item, rev: Option<&str>) -> String {
        match item.code.as_str() {
            "?" => match read_file_string(&self.worktree.join(&item.file)) {
                Some(content) if content.len() <= MAX_PATCH_BYTES => patch::add_patch(&item.file, &content),
                _ => patch::empty_patch(&item.file),
            },
            "!" => match self.cat(rev.unwrap_or("."), &item.file).await {
                Some(content) if content.len() <= MAX_PATCH_BYTES => patch::delete_patch(&item.file, &content),
                _ => patch::empty_patch(&item.file),
            },
            _ => patch::empty_patch(&item.file),
        }
    }

    /// `hg status -0 [--rev <rev>] <scope>`; `[]` when it fails.
    async fn status_names(&self, rev: Option<&str>) -> Vec<Item> {
        let mut args = vec!["status".to_string(), "-0".to_string()];
        if let Some(rev) = rev {
            args.extend(["--rev".to_string(), rev.to_string()]);
        }
        args.push(self.scope.clone());
        let out = run(&self.worktree, args, None).await;
        if out.exit_code != 0 {
            return vec![];
        }
        out.text
            .split('\0')
            .filter(|entry| !entry.is_empty())
            .filter_map(|entry| {
                let code: String = entry.chars().take(1).collect();
                let file: String = entry.chars().skip(2).collect();
                if file.is_empty() {
                    return None;
                }
                Some(Item { status: kind(&code), file, code })
            })
            .collect()
    }

    /// `hg diff --git --unified <context> [-r <rev>] <scope>`, capped at the total patch budget.
    async fn diff_patch(&self, rev: Option<&str>, context: u64) -> Patch {
        let mut args = vec!["diff".to_string(), "--git".to_string(), "--unified".to_string(), context.to_string()];
        if let Some(rev) = rev {
            args.extend(["-r".to_string(), rev.to_string()]);
        }
        args.push(self.scope.clone());
        let out = run(&self.worktree, args, Some(MAX_TOTAL_PATCH_BYTES)).await;
        if out.exit_code != 0 {
            return Patch::default();
        }
        Patch { text: out.text, truncated: out.truncated }
    }

    /// `hg branch`: the working directory's branch.
    pub async fn branch(&self) -> Option<String> {
        let out = run(&self.worktree, vec!["branch".into()], None).await;
        if out.exit_code != 0 {
            return None;
        }
        Some(out.text.trim().to_string()).filter(|b| !b.is_empty())
    }

    /// The common ancestor of the working directory's parent and `default`.
    async fn ancestor(&self) -> Option<String> {
        let args = ["log", "-r", "ancestor(., default)", "-T", "{node}"].map(String::from).to_vec();
        let out = run(&self.worktree, args, None).await;
        if out.exit_code != 0 {
            return None;
        }
        Some(out.text.trim().to_string()).filter(|n| !n.is_empty())
    }

    /// `hg cat -r <rev> -- <file>`; `None` when it fails or exceeds the per-file budget.
    async fn cat(&self, rev: &str, file: &str) -> Option<String> {
        let args = vec!["cat".into(), "-r".into(), rev.to_string(), "--".into(), file.to_string()];
        let out = run(&self.worktree, args, Some(MAX_PATCH_BYTES)).await;
        if out.exit_code != 0 || out.truncated {
            return None;
        }
        Some(out.text)
    }
}

/// `FileSystem.readFileString`: `TextDecoder("utf-8")`, so invalid sequences become U+FFFD and a
/// leading byte-order mark is dropped.
fn read_file_string(path: &Path) -> Option<String> {
    let bytes = std::fs::read(path).ok()?;
    let text = String::from_utf8_lossy(&bytes);
    Some(text.strip_prefix('\u{feff}').unwrap_or(&text).to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn status_codes() {
        assert_eq!(kind("A"), Kind::Added);
        assert_eq!(kind("?"), Kind::Added);
        assert_eq!(kind("R"), Kind::Deleted);
        assert_eq!(kind("!"), Kind::Deleted);
        assert_eq!(kind("M"), Kind::Modified);
    }

    #[test]
    fn scope_is_relative_to_the_worktree() {
        assert_eq!(HgVcs::new("/r", "/r").scope, ".");
        // `path.relative`: platform separators.
        assert_eq!(HgVcs::new("/r/a/b", "/r").scope, format!("a{}b", std::path::MAIN_SEPARATOR));
    }

    #[tokio::test]
    async fn rejects_unsupported_review_modes_without_hg() {
        let vcs = HgVcs::new("/nonexistent-hg-dir", "/nonexistent-hg-dir");
        let error = vcs.diff(Mode::Committed, &DiffOptions::default()).await.unwrap_err();
        assert_eq!(error.message, UNSUPPORTED_REVIEW_MESSAGE);
        let options = DiffOptions { base: Some("release".into()), ..Default::default() };
        assert_eq!(vcs.diff(Mode::Branch, &options).await.unwrap_err().message, UNSUPPORTED_REVIEW_MESSAGE);
    }
}
