//! Port of `packages/core/src/git.ts`: repository discovery, remote/history queries, sync for the
//! repository cache, worktrees, and the snapshot store (index refresh, trees, diffs, restore, packing).

use std::collections::{HashMap, HashSet};
use std::future::Future;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use futures::StreamExt;
use once_cell::sync::Lazy;
use parking_lot::Mutex;
use regex::Regex;
use serde::{Deserialize, Serialize};

use oc_schema::types::{FileDiff, FileStatus};

use crate::patch::{self, Patch};
use crate::paths::resolve_git_path;
use crate::process::{self, RunOptions, RunResult};

/// Included from `$GIT_DIR/config` via `include.path`; OpenCode owns this file entirely.
pub const SNAPSHOT_CONFIG_FILE: &str = "opencode.gitconfig";
pub const SNAPSHOT_CONFIG_INCLUDE: &str = "[include]\n\tpath = opencode.gitconfig\n";
pub const SNAPSHOT_CONFIG: &str = "[core]\n\tautocrlf = false\n\tlongpaths = true\n\tsymlinks = true\n\tfsmonitor = false\n\tuntrackedCache = true\n\t# A split index cannot name its shared file once manyFiles skips index checksums.\n\tsplitIndex = false\n[feature]\n\tmanyFiles = true\n[index]\n\tversion = 4\n\tthreads = true\n";

pub const TEMPORARY_INDEX_PREFIX: &str = "index.opencode-";
/// Room for paths on one Git command line after the Git path, repository flags, and quoting.
pub const ARGUMENT_BUDGET: usize = if cfg!(windows) { 24_000 } else { 128 * 1024 };
pub const LOOSE_OBJECT_LIMIT: usize = 2048;
pub const PACK_COUNT_LIMIT: usize = 16;
const HOUR_MS: u128 = 60 * 60 * 1000;

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Repository {
    pub worktree: PathBuf,
    pub git_directory: PathBuf,
    pub common_directory: PathBuf,
}

/// A Git tree object ID (40 or 64 hex characters).
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
#[derive(Default)]
pub struct TreeId(pub String);

impl TreeId {
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for TreeId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl From<&str> for TreeId {
    fn from(value: &str) -> Self {
        TreeId(value.to_string())
    }
}

impl From<String> for TreeId {
    fn from(value: String) -> Self {
        TreeId(value)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Operation {
    Clone,
    Fetch,
    Checkout,
    Reset,
    Create,
    Refresh,
    WriteTree,
    ListFiles,
    Diff,
    Restore,
    Pack,
}

impl Operation {
    pub fn as_str(self) -> &'static str {
        match self {
            Operation::Clone => "clone",
            Operation::Fetch => "fetch",
            Operation::Checkout => "checkout",
            Operation::Reset => "reset",
            Operation::Create => "create",
            Operation::Refresh => "refresh",
            Operation::WriteTree => "write_tree",
            Operation::ListFiles => "list_files",
            Operation::Diff => "diff",
            Operation::Restore => "restore",
            Operation::Pack => "pack",
        }
    }
}

/// On Windows, Git exits non-zero instead of reusing a loose object that another process is finalizing at the same
/// moment. Objects are content-addressed, so one of at most two retries (50 ms apart) finds that copy in place.
/// Only a Git exit is retried, never a failure to start Git, and other platforms fail on the first attempt
/// (`retryConcurrentObjectWrite`, bc5ebf00a).
async fn retry_concurrent_object_write<T, F, Fut>(retry: bool, mut run: F) -> Result<T, OperationError>
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = Result<T, OperationError>>,
{
    let mut retries = 0;
    loop {
        match run().await {
            Err(error) if retry && retries < 2 && error.cause.is_none() => {
                retries += 1;
                tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            }
            result => return result,
        }
    }
}

/// `Git.OperationError`. `cause` is set when the failure did not come from Git's exit status
/// (spawn failure, filesystem error).
#[derive(Debug, Clone, thiserror::Error)]
#[error("{message}")]
pub struct OperationError {
    pub operation: Operation,
    pub message: String,
    pub directory: Option<PathBuf>,
    pub cause: Option<String>,
}

impl OperationError {
    pub fn new(operation: Operation, directory: Option<&Path>, message: impl Into<String>) -> Self {
        OperationError { operation, message: message.into(), directory: directory.map(Path::to_path_buf), cause: None }
    }
    fn caused(operation: Operation, directory: &Path, message: impl Into<String>, cause: impl ToString) -> Self {
        OperationError {
            operation,
            message: message.into(),
            directory: Some(directory.to_path_buf()),
            cause: Some(cause.to_string()),
        }
    }
    pub const TAG: &'static str = "Git.OperationError";
}

/// `Git.CommandError`: Git could not answer (the spawn failed or it exited with an unexpected status).
#[derive(Debug, Clone, thiserror::Error)]
#[error("git {} failed", args.join(" "))]
pub struct CommandError {
    pub args: Vec<String>,
    pub exit_code: Option<i32>,
    pub cause: Option<String>,
}

impl CommandError {
    pub const TAG: &'static str = "Git.CommandError";
}

/// `stamp`: `<type>:<ino>:<size>:<mtime ms>` of a path, or `missing`.
fn stamp(target: &Path) -> String {
    let Ok(info) = std::fs::metadata(target) else { return s("missing") };
    let kind = if info.is_file() {
        "File"
    } else if info.is_dir() {
        "Directory"
    } else {
        "Other"
    };
    #[cfg(unix)]
    let ino = {
        use std::os::unix::fs::MetadataExt;
        info.ino().to_string()
    };
    #[cfg(not(unix))]
    let ino = String::new();
    format!("{kind}:{ino}:{}:{}", info.len(), mtime_ms(&info))
}

fn read_text(target: &Path) -> Option<String> {
    std::fs::read_to_string(target).ok()
}

static GITDIR_LINE: Lazy<Regex> = Lazy::new(|| Regex::new(r"(?m)^gitdir:\s*(.+)$").unwrap());

/// `repo.fingerprint`. Covers linked worktrees (gitdir file + commondir), HEAD and the ref it
/// points at, loose/packed/reftable refs, repository/worktree/global/XDG/system config, and the
/// cached `opencode` project id, so external `git init`, a first commit or `git remote` edits
/// change the stamp.
pub fn fingerprint(dot_git: &Path) -> Option<String> {
    let dot = std::fs::metadata(dot_git).ok()?;
    if !dot.is_file() && !dot.is_dir() {
        return None;
    }
    let git_file = if dot.is_file() {
        let text = read_text(dot_git)?;
        Some(GITDIR_LINE.captures(&text)?.get(1)?.as_str().trim().to_string())
    } else {
        None
    };
    let git_dir = match git_file.filter(|file| !file.is_empty()) {
        Some(file) => {
            let resolved = crate::paths::resolve(dot_git.parent().unwrap_or(Path::new("/")), &file);
            std::fs::canonicalize(&resolved).unwrap_or(resolved)
        }
        None => dot_git.to_path_buf(),
    };
    let common_dir = match read_text(&git_dir.join("commondir")).map(|text| text.trim().to_string()) {
        Some(commondir) if !commondir.is_empty() => {
            let resolved = crate::paths::resolve(&git_dir, &commondir);
            std::fs::canonicalize(&resolved).unwrap_or(resolved)
        }
        _ => git_dir.clone(),
    };
    let head = read_text(&git_dir.join("HEAD"))?.trim().to_string();
    let reference = head.strip_prefix("ref:").map(|rest| rest.trim().to_string());
    let env = |key: &str| std::env::var(key).ok().filter(|value| !value.is_empty());
    let home = env("HOME").map(PathBuf::from).unwrap_or_default();
    let nosystem = ["1", "true", "yes", "on"]
        .contains(&std::env::var("GIT_CONFIG_NOSYSTEM").unwrap_or_default().to_lowercase().as_str());
    let mut files = vec![
        git_dir.join("HEAD"),
        common_dir.join("config"),
        common_dir.join("config.worktree"),
        git_dir.join("config.worktree"),
        common_dir.join("packed-refs"),
        common_dir.join("reftable").join("tables.list"),
        common_dir.join("opencode"),
    ];
    if let Some(reference) = &reference {
        files.push(common_dir.join(reference));
        files.push(git_dir.join(reference));
    }
    files.push(env("GIT_CONFIG_GLOBAL").map(PathBuf::from).unwrap_or_else(|| home.join(".gitconfig")));
    files.push(
        env("XDG_CONFIG_HOME").map(PathBuf::from).unwrap_or_else(|| home.join(".config")).join("git").join("config"),
    );
    if !nosystem {
        files.push(env("GIT_CONFIG_SYSTEM").map(PathBuf::from).unwrap_or_else(|| PathBuf::from("/etc/gitconfig")));
    }
    let stamps: Vec<String> = std::iter::once(dot_git).chain(files.iter().map(PathBuf::as_path)).map(stamp).collect();
    let parts: Vec<String> = [p(&git_dir), p(&common_dir), head]
        .into_iter()
        .chain(files.iter().map(|file| p(file)))
        .chain(stamps)
        .collect();
    Some(parts.join("\0"))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum WorktreeKind {
    Main,
    Linked,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Worktree {
    pub directory: PathBuf,
    pub kind: WorktreeKind,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum WorktreeOperation {
    Create,
    Remove,
    List,
}

/// `Git.WorktreeError`.
#[derive(Debug, Clone, thiserror::Error)]
#[error("{message}")]
pub struct WorktreeError {
    pub operation: WorktreeOperation,
    pub message: String,
    pub directory: Option<PathBuf>,
    pub force_required: Option<bool>,
    pub cause: Option<String>,
}

impl WorktreeError {
    pub const TAG: &'static str = "Git.WorktreeError";
}

#[derive(Debug, Clone)]
pub struct CaptureInput {
    pub repository: Repository,
    pub scopes: Vec<String>,
    /// Source repository whose ignore rules decide which paths are recorded. Its index also
    /// rebuilds an unreadable snapshot index without rehashing every tracked file.
    pub ignores: Option<Repository>,
    pub maximum_untracked_file_bytes: Option<u64>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RefreshResult {
    pub skipped: Vec<String>,
}

#[derive(Debug, Clone, Default)]
pub struct CloneInput {
    pub remote: String,
    pub directory: PathBuf,
    pub branch: Option<String>,
    pub depth: Option<u32>,
}

#[derive(Debug, Clone)]
pub struct FetchBranchInput {
    pub remote: Option<String>,
    pub branch: String,
    pub force: Option<bool>,
}

#[derive(Debug, Clone)]
pub struct CheckoutInput {
    pub remote: Option<String>,
    pub branch: String,
    pub reset: Option<bool>,
}

#[derive(Debug, Clone, Default)]
pub struct DiffInput {
    pub from: TreeId,
    pub to: TreeId,
    pub context: Option<u64>,
    pub paths: Option<Vec<String>>,
}


struct Changes {
    tracked: Vec<String>,
    untracked: Vec<String>,
}

#[derive(Clone)]
struct LastCapture {
    fingerprint: String,
    tree: TreeId,
}

struct OpOutput {
    text: String,
    truncated: bool,
}

#[derive(Default)]
struct RepoOptions<'a> {
    stdin: Option<Vec<u8>>,
    max_output_bytes: Option<usize>,
    index: Option<&'a Path>,
}

#[derive(Default)]
struct Inner {
    locks: Mutex<HashMap<PathBuf, Arc<tokio::sync::Mutex<()>>>>,
    /// A clean capture returns the tree last written from the exact index it scanned.
    last_captures: Mutex<HashMap<PathBuf, LastCapture>>,
    prepared_stores: Mutex<HashSet<PathBuf>>,
    /// Captures queued behind the store lock that have not started yet, by capture key.
    pending_captures: Mutex<HashMap<String, PendingCapture>>,
    pending_ids: std::sync::atomic::AtomicU64,
    /// Untracked files skipped for size, per store, by path, with the stamp they had when skipped.
    oversized: Mutex<HashMap<PathBuf, HashMap<String, String>>>,
}

type CaptureResult = Option<Result<TreeId, OperationError>>;

struct PendingCapture {
    id: u64,
    receiver: tokio::sync::watch::Receiver<CaptureResult>,
}

/// Forgets a pending capture once it starts (or is dropped), unless a newer one replaced it.
struct PendingGuard {
    inner: Arc<Inner>,
    key: String,
    id: u64,
}

impl PendingGuard {
    fn release(&self) {
        let mut pending = self.inner.pending_captures.lock();
        if pending.get(&self.key).is_some_and(|entry| entry.id == self.id) {
            pending.remove(&self.key);
        }
    }
}

impl Drop for PendingGuard {
    fn drop(&mut self) {
        self.release();
    }
}

/// The `Git` service. Cheap to clone; clones share the in-process locks and capture caches.
#[derive(Clone, Default)]
pub struct Git {
    inner: Arc<Inner>,
}

fn s(value: &str) -> String {
    value.to_string()
}

fn p(value: &Path) -> String {
    value.to_string_lossy().into_owned()
}

/// Split NUL-terminated git output into its records.
pub fn nuls(text: &str) -> Vec<String> {
    text.split('\0').filter(|item| !item.is_empty()).map(str::to_string).collect()
}

/// Pathspec magic that matches exactly one path.
pub fn literal_pathspec(file: &str) -> String {
    format!(":(literal){file}")
}

fn js_len(value: &str) -> usize {
    value.encode_utf16().count()
}

fn now_ms() -> u128 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis()).unwrap_or(0)
}

fn mtime_ms(meta: &std::fs::Metadata) -> u128 {
    meta.modified().ok().and_then(|m| m.duration_since(UNIX_EPOCH).ok()).map(|d| d.as_millis()).unwrap_or(0)
}

/// `${process.pid}-${Math.random().toString(36).slice(2)}`
pub fn unique_suffix() -> String {
    const ALPHABET: &[u8] = b"0123456789abcdefghijklmnopqrstuvwxyz";
    let random: String = (0..11).map(|_| ALPHABET[rand::random_range(0..36)] as char).collect();
    format!("{}-{}", std::process::id(), random)
}

static TREE_ID: Lazy<Regex> = Lazy::new(|| Regex::new(r"^[0-9a-f]{40,64}$").unwrap());
static TREE_ENTRY_PREFIX: Lazy<Regex> = Lazy::new(|| Regex::new(r"^\d+\s+\w+\s+[0-9a-f]+\t").unwrap());
static TREE_ENTRY: Lazy<Regex> = Lazy::new(|| Regex::new(r"(?s)^\d+ \w+ [0-9a-f]+\t(.*)$").unwrap());
static FANOUT: Lazy<Regex> = Lazy::new(|| Regex::new(r"^[0-9a-f]{2}$").unwrap());
static LOOSE: Lazy<Regex> = Lazy::new(|| Regex::new(r"^[0-9a-f]{38}$|^[0-9a-f]{62}$").unwrap());
static FORCE_REQUIRED: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"(?i)contains modified or untracked files|is dirty").unwrap());

fn repository_args(repository: &Repository, args: Vec<String>) -> Vec<String> {
    let mut out = vec![s("--git-dir"), p(&repository.git_directory), s("--work-tree"), p(&repository.worktree)];
    out.extend(args);
    out
}

async fn execute(cwd: &Path, args: Vec<String>) -> std::io::Result<RunResult> {
    process::run(cwd, &args, RunOptions::default()).await
}

/// `run()`: any spawn failure becomes exit code 1 with empty output.
async fn run(cwd: &Path, args: Vec<String>) -> RunResult {
    execute(cwd, args).await.unwrap_or(RunResult {
        exit_code: 1,
        stdout: Vec::new(),
        stderr: Vec::new(),
        stdout_truncated: false,
    })
}

async fn exists(path: &Path) -> bool {
    tokio::fs::try_exists(path).await.unwrap_or(false)
}

/// `fs.remove(path, { recursive: true, force: true })`.
async fn remove_recursive(path: &Path) -> std::io::Result<()> {
    match tokio::fs::symlink_metadata(path).await {
        Ok(meta) if meta.is_dir() => tokio::fs::remove_dir_all(path).await,
        Ok(_) => tokio::fs::remove_file(path).await,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error),
    }
}

async fn remove_file_force(path: &Path) {
    let _ = tokio::fs::remove_file(path).await;
}

/// A copied index must keep the timestamp Git uses to detect racily clean entries.
fn copy_index(from: &Path, to: &Path) -> std::io::Result<()> {
    let info = std::fs::metadata(from)?;
    std::fs::copy(from, to)?;
    if let Ok(mtime) = info.modified() {
        let atime = info.accessed().unwrap_or(mtime);
        if let Ok(file) = std::fs::File::options().write(true).open(to) {
            let _ = file.set_times(std::fs::FileTimes::new().set_accessed(atime).set_modified(mtime));
        }
    }
    Ok(())
}

/// Git only replaces an index by renaming a new file into place, so every write changes its inode.
fn index_fingerprint(file: &Path) -> Option<String> {
    let info = std::fs::metadata(file).ok()?;
    #[cfg(unix)]
    let ino = {
        use std::os::unix::fs::MetadataExt;
        info.ino().to_string()
    };
    #[cfg(not(unix))]
    let ino = String::new();
    Some(format!("{}:{}:{}", ino, info.len(), mtime_ms(&info)))
}

/// Removes a temporary file when dropped, including on cancellation.
struct TempGuard(PathBuf);

impl Drop for TempGuard {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

struct TempDirGuard(PathBuf);

impl Drop for TempDirGuard {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// Paths whose restore operations commute: none inside another, and every segment printable
/// ASCII that no platform rewrites.
pub fn can_batch_restore(files: &[String]) -> bool {
    let canonical = files.iter().all(|file| {
        file.split('/').all(|part| {
            !part.is_empty()
                && part.bytes().all(|b| (0x20..=0x7e).contains(&b))
                && !part.contains(['\\', ':'])
                && !part.ends_with(['.', ' '])
        })
    });
    if !canonical {
        return false;
    }
    let folded: HashSet<String> = files.iter().map(|f| f.to_lowercase()).collect();
    if folded.len() != files.len() {
        return false;
    }
    files.iter().all(|file| {
        let lower = file.to_lowercase();
        let parts: Vec<&str> = lower.split('/').collect();
        (1..parts.len()).all(|index| !folded.contains(&parts[..index].join("/")))
    })
}

/// Object IDs from a version 2 pack index; `None` for version 1 indexes and for packs with
/// 64-bit offsets, whose hash length this size check cannot derive.
pub fn pack_index_object_ids(bytes: &[u8]) -> Option<Vec<String>> {
    let u32_at = |offset: usize| u32::from_be_bytes(bytes[offset..offset + 4].try_into().unwrap());
    if bytes.len() < 1032 || u32_at(0) != 0xff74_4f63 || u32_at(4) != 2 {
        return None;
    }
    let count = u32_at(8 + 255 * 4) as i64;
    let numerator = bytes.len() as i64 - 1032 - 8 * count;
    if numerator < 0 || numerator % (count + 2) != 0 {
        return None;
    }
    let hash = (numerator / (count + 2)) as usize;
    if hash != 20 && hash != 32 {
        return None;
    }
    Some(
        (0..count as usize)
            .map(|index| {
                let start = 1032 + index * hash;
                bytes[start..start + hash].iter().map(|b| format!("{b:02x}")).collect()
            })
            .collect(),
    )
}

/// Groups sorted like Git's output, each fitting the argument budget (3 extra per path for quoting).
pub fn path_groups(paths: &[String]) -> Vec<Vec<String>> {
    let mut sorted = paths.to_vec();
    sorted.sort_by(|a, b| a.as_bytes().cmp(b.as_bytes()));
    let mut groups: Vec<Vec<String>> = Vec::new();
    let mut length = 0usize;
    for file in sorted {
        let cost = js_len(&file) + 3;
        match groups.last_mut() {
            Some(current) if length + cost <= ARGUMENT_BUDGET => {
                current.push(file);
                length += cost;
            }
            _ => {
                groups.push(vec![file]);
                length = cost;
            }
        }
    }
    groups
}

impl Git {
    pub fn new() -> Self {
        Self::default()
    }

    fn lock_for(&self, repository: &Repository) -> Arc<tokio::sync::Mutex<()>> {
        self.inner.locks.lock().entry(repository.git_directory.clone()).or_default().clone()
    }

    // ---------------------------------------------------------------- repo

    /// `repo.discover`: nearest `.git` above `input`, then `rev-parse --git-dir --git-common-dir --show-toplevel`.
    pub async fn discover(&self, input: &Path) -> Option<Repository> {
        let mut current = input.to_path_buf();
        let dotgit = loop {
            let candidate = current.join(".git");
            if exists(&candidate).await {
                break candidate;
            }
            match current.parent() {
                Some(parent) if parent != current => current = parent.to_path_buf(),
                _ => return None,
            }
        };
        let cwd = dotgit.parent()?.to_path_buf();
        let result = run(&cwd, vec![s("rev-parse"), s("--git-dir"), s("--git-common-dir"), s("--show-toplevel")]).await;
        let text = result.text();
        let lines: Vec<&str> = text.split('\n').map(|line| line.strip_suffix('\r').unwrap_or(line)).collect();
        let git_dir = lines.first().copied().unwrap_or("");
        let common_dir = lines.get(1).copied().unwrap_or("");
        let top_level = lines.get(2).copied().unwrap_or("");
        if git_dir.is_empty() || common_dir.is_empty() {
            return None;
        }
        Some(Repository {
            worktree: if top_level.is_empty() { cwd.clone() } else { resolve_git_path(&cwd, top_level) },
            git_directory: resolve_git_path(&cwd, git_dir),
            common_directory: resolve_git_path(&cwd, common_dir),
        })
    }

    async fn operation(&self, operation: Operation, directory: &Path, args: Vec<String>) -> Result<(), OperationError> {
        let result = execute(directory, args)
            .await
            .map_err(|cause| OperationError::caused(operation, directory, cause.to_string(), &cause))?;
        if result.exit_code == 0 {
            return Ok(());
        }
        Err(OperationError::new(operation, Some(directory), failure_message(&result, operation)))
    }

    /// `repo.clone`: `git clone --depth <depth=100> [--branch <b>] -- <remote> <dir>`.
    pub async fn clone_repository(&self, input: CloneInput) -> Result<Repository, OperationError> {
        let parent = input.directory.parent().map(Path::to_path_buf).unwrap_or_else(|| PathBuf::from("/"));
        let mut args = vec![s("clone"), s("--depth"), input.depth.unwrap_or(100).to_string()];
        if let Some(branch) = input.branch.as_deref().filter(|b| !b.is_empty()) {
            args.extend([s("--branch"), s(branch)]);
        }
        args.extend([s("--"), input.remote.clone(), p(&input.directory)]);
        self.operation(Operation::Clone, &parent, args).await?;
        match self.discover(&input.directory).await {
            Some(repository) => Ok(repository),
            None => Err(OperationError::new(
                Operation::Clone,
                Some(&input.directory),
                "Cloned repository could not be opened",
            )),
        }
    }

    /// `repo.create`: initialize a snapshot store. A new store is initialized in a temporary
    /// sibling directory and renamed into place.
    pub async fn create(
        &self,
        worktree: &Path,
        git_directory: &Path,
        seed: Option<&Repository>,
    ) -> Result<Repository, OperationError> {
        let repository = Repository {
            worktree: worktree.to_path_buf(),
            git_directory: git_directory.to_path_buf(),
            common_directory: git_directory.to_path_buf(),
        };
        if exists(&git_directory.join("HEAD")).await {
            self.init_repository(worktree, git_directory, git_directory, seed).await?;
            return Ok(repository);
        }
        let temporary = PathBuf::from(format!("{}.init-{}", git_directory.display(), unique_suffix()));
        let _guard = TempDirGuard(temporary.clone());
        self.init_repository(worktree, git_directory, &temporary, seed).await?;
        let renamed = tokio::fs::rename(&temporary, git_directory).await;
        if renamed.is_ok() || exists(&git_directory.join("HEAD")).await {
            return Ok(repository);
        }
        let cause = renamed.err().map(|e| e.to_string()).unwrap_or_default();
        Err(OperationError::caused(Operation::Create, git_directory, "Failed to move Git storage into place", cause))
    }

    async fn init_repository(
        &self,
        worktree: &Path,
        git_directory: &Path,
        directory: &Path,
        seed: Option<&Repository>,
    ) -> Result<(), OperationError> {
        let error = |message: &'static str| {
            move |cause: std::io::Error| OperationError::caused(Operation::Create, git_directory, message, cause)
        };
        tokio::fs::create_dir_all(directory).await.map_err(error("Failed to create Git storage"))?;
        let temp = Repository {
            worktree: worktree.to_path_buf(),
            git_directory: directory.to_path_buf(),
            common_directory: directory.to_path_buf(),
        };
        self.repository_operation(Operation::Create, &temp, vec![s("init")], RepoOptions::default()).await?;
        async {
            tokio::fs::write(directory.join(SNAPSHOT_CONFIG_FILE), SNAPSHOT_CONFIG).await?;
            let config = directory.join("config");
            let current = tokio::fs::read_to_string(&config).await?;
            if current.contains(SNAPSHOT_CONFIG_INCLUDE) {
                return Ok(());
            }
            let addition =
                format!("{}{}", if current.ends_with('\n') { "\n" } else { "\n\n" }, SNAPSHOT_CONFIG_INCLUDE);
            use tokio::io::AsyncWriteExt;
            let mut file = tokio::fs::OpenOptions::new().append(true).open(&config).await?;
            file.write_all(addition.as_bytes()).await?;
            file.flush().await
        }
        .await
        .map_err(error("Failed to configure Git storage"))?;
        let Some(seed) = seed else { return Ok(()) };
        let info = directory.join("objects").join("info");
        tokio::fs::create_dir_all(&info).await.map_err(error("Failed to configure shared Git objects"))?;
        tokio::fs::write(info.join("alternates"), format!("{}\n", p(&seed.common_directory.join("objects"))))
            .await
            .map_err(error("Failed to configure shared Git objects"))?;
        let index = directory.join("index");
        let store = Repository {
            worktree: worktree.to_path_buf(),
            git_directory: directory.to_path_buf(),
            common_directory: directory.to_path_buf(),
        };
        if self.seed_index(&store, seed, &index).await.is_err() {
            remove_file_force(&index).await;
        }
        Ok(())
    }

    /// Copy the source index into the store. A split index names a shared index file in its own
    /// Git directory, so those files are linked in just long enough to rewrite the copy as a
    /// complete index (`seedIndex`).
    async fn seed_index(&self, store: &Repository, source: &Repository, target: &Path) -> Result<(), OperationError> {
        let failed = |message: &'static str| {
            move |cause: std::io::Error| OperationError::caused(Operation::Refresh, &store.git_directory, message, cause)
        };
        copy_index(&source.git_directory.join("index"), target).map_err(failed("Failed to copy the source index"))?;
        let mut shared: Vec<String> = std::fs::read_dir(&source.git_directory)
            .map_err(failed("Failed to read the source Git directory"))?
            .flatten()
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .filter(|name| name.starts_with("sharedindex."))
            .collect();
        if shared.is_empty() {
            return Ok(());
        }
        shared.sort();
        let result = async {
            for entry in &shared {
                let from = source.git_directory.join(entry);
                let to = store.git_directory.join(entry);
                if std::fs::hard_link(&from, &to).is_err() {
                    std::fs::copy(&from, &to).map_err(failed("Failed to link the shared index"))?;
                }
            }
            self.repository_operation(
                Operation::Refresh,
                store,
                vec![s("update-index"), s("--no-split-index")],
                RepoOptions { index: Some(target), ..Default::default() },
            )
            .await
            .map(|_| ())
        }
        .await;
        for entry in &shared {
            remove_file_force(&store.git_directory.join(entry)).await;
        }
        result
    }

    // ---------------------------------------------------------------- remote / history

    /// `repo.fingerprint`: spawn-free stamp of the metadata that `discover`, `remote` and
    /// `root_commits` read for the `.git` entry at `dot_git`; `None` when it isn't a readable
    /// repository.
    pub async fn fingerprint(&self, dot_git: &Path) -> Option<String> {
        fingerprint(dot_git)
    }

    async fn command(&self, cwd: &Path, args: Vec<String>) -> Result<RunResult, CommandError> {
        execute(cwd, args.clone()).await.map_err(|cause| CommandError { args, exit_code: None, cause: Some(cause.to_string()) })
    }

    /// `remote.get`: `git remote get-url <name=origin>`. `Ok(None)` when the remote doesn't exist
    /// (exit 2); any other failure is a [`CommandError`].
    pub async fn remote(&self, repository: &Repository, name: Option<&str>) -> Result<Option<String>, CommandError> {
        let args = vec![s("remote"), s("get-url"), s(name.unwrap_or("origin"))];
        let result = self.command(&repository.worktree, args.clone()).await?;
        if result.exit_code == 2 {
            return Ok(None);
        }
        if result.exit_code != 0 {
            return Err(CommandError { args, exit_code: Some(result.exit_code), cause: None });
        }
        Ok(Some(result.text().trim().to_string()).filter(|v| !v.is_empty()))
    }

    /// `history.rootCommits`: `git rev-list --max-parents=0 HEAD`, sorted; empty when HEAD is unborn.
    pub async fn root_commits(&self, repository: &Repository) -> Result<Vec<String>, CommandError> {
        let args = vec![s("rev-list"), s("--max-parents=0"), s("HEAD")];
        let result = self.command(&repository.worktree, args.clone()).await?;
        if result.exit_code == 0 {
            let mut out: Vec<String> =
                result.text().split('\n').map(|l| l.trim().to_string()).filter(|l| !l.is_empty()).collect();
            out.sort();
            return Ok(out);
        }
        // `rev-parse -q --verify` exits 1 only when HEAD doesn't resolve, i.e. it is unborn.
        let verify = self.command(&repository.worktree, vec![s("rev-parse"), s("-q"), s("--verify"), s("HEAD")]).await?;
        if verify.exit_code == 1 {
            return Ok(Vec::new());
        }
        Err(CommandError { args, exit_code: Some(result.exit_code), cause: None })
    }

    /// `history.head`: `git rev-parse HEAD`.
    pub async fn head(&self, repository: &Repository) -> Option<String> {
        let result = run(&repository.worktree, vec![s("rev-parse"), s("HEAD")]).await;
        if result.exit_code != 0 {
            return None;
        }
        Some(result.text().trim().to_string()).filter(|v| !v.is_empty())
    }

    /// `history.branch`: `git symbolic-ref --quiet --short HEAD`.
    pub async fn branch(&self, repository: &Repository) -> Option<String> {
        let result = run(&repository.worktree, vec![s("symbolic-ref"), s("--quiet"), s("--short"), s("HEAD")]).await;
        if result.exit_code != 0 {
            return None;
        }
        Some(result.text().trim().to_string()).filter(|v| !v.is_empty())
    }

    /// `history.defaultRemoteBranch`: `git symbolic-ref refs/remotes/<remote>/HEAD`.
    pub async fn default_remote_branch(&self, repository: &Repository, remote: Option<&str>) -> Option<String> {
        let remote = remote.unwrap_or("origin");
        let result =
            run(&repository.worktree, vec![s("symbolic-ref"), format!("refs/remotes/{remote}/HEAD")]).await;
        if result.exit_code != 0 {
            return None;
        }
        let text = result.text();
        let trimmed = text.trim();
        let prefix = format!("refs/remotes/{remote}/");
        let value = trimmed.strip_prefix(&prefix).unwrap_or(trimmed);
        Some(value.to_string()).filter(|v| !v.is_empty())
    }

    // ---------------------------------------------------------------- sync

    /// `sync.fetchRemotes`: `git fetch --all [--prune]`.
    pub async fn fetch_remotes(&self, repository: &Repository, prune: Option<bool>) -> Result<(), OperationError> {
        let mut args = vec![s("fetch"), s("--all")];
        if prune != Some(false) {
            args.push(s("--prune"));
        }
        self.operation(Operation::Fetch, &repository.worktree, args).await
    }

    /// `sync.fetchBranch`: `git fetch <remote> [+]refs/heads/<b>:refs/remotes/<remote>/<b>`.
    pub async fn fetch_branch(&self, repository: &Repository, input: FetchBranchInput) -> Result<(), OperationError> {
        let remote = input.remote.unwrap_or_else(|| s("origin"));
        let spec = format!("refs/heads/{}:refs/remotes/{}/{}", input.branch, remote, input.branch);
        let spec = if input.force == Some(false) { spec } else { format!("+{spec}") };
        self.operation(Operation::Fetch, &repository.worktree, vec![s("fetch"), remote, spec]).await
    }

    /// `sync.checkoutRemoteBranch`: `git checkout -B <b> <remote>/<b>` (or `git checkout <b>`).
    pub async fn checkout_remote_branch(
        &self,
        repository: &Repository,
        input: CheckoutInput,
    ) -> Result<(), OperationError> {
        let remote = input.remote.unwrap_or_else(|| s("origin"));
        let mut args = vec![s("checkout")];
        if input.reset == Some(false) {
            args.push(input.branch);
        } else {
            args.extend([s("-B"), input.branch.clone(), format!("{remote}/{}", input.branch)]);
        }
        self.operation(Operation::Checkout, &repository.worktree, args).await
    }

    /// `sync.resetHard`: `git reset --hard <revision>`.
    pub async fn reset_hard(&self, repository: &Repository, revision: &str) -> Result<(), OperationError> {
        self.operation(Operation::Reset, &repository.worktree, vec![s("reset"), s("--hard"), s(revision)]).await
    }

    // ---------------------------------------------------------------- repository-scoped commands

    async fn repository_operation(
        &self,
        operation: Operation,
        repository: &Repository,
        args: Vec<String>,
        options: RepoOptions<'_>,
    ) -> Result<OpOutput, OperationError> {
        let mut env = Vec::new();
        if let Some(index) = options.index {
            env.push(("GIT_INDEX_FILE", p(index)));
        }
        let result = process::run(
            &repository.worktree,
            &repository_args(repository, args),
            RunOptions { env, stdin: options.stdin, max_output_bytes: options.max_output_bytes },
        )
        .await
        .map_err(|cause| OperationError::caused(operation, &repository.worktree, cause.to_string(), &cause))?;
        if result.exit_code == 0 {
            return Ok(OpOutput { text: result.text(), truncated: result.stdout_truncated });
        }
        Err(OperationError::new(operation, Some(&repository.worktree), failure_message(&result, operation)))
    }

    /// Both commands only read the index, so they never take `index.lock`.
    async fn list_changes(
        &self,
        repository: &Repository,
        scope: &str,
        index: Option<&Path>,
        exclude_from: Option<&Path>,
    ) -> Result<Changes, OperationError> {
        let mut others = vec![s("ls-files"), s("--others")];
        if let Some(exclude_from) = exclude_from {
            others.push(format!("--exclude-from={}", p(exclude_from)));
        }
        others.extend([s("--exclude-standard"), s("-z"), s("--"), literal_pathspec(scope)]);
        let (tracked, untracked) = tokio::try_join!(
            self.list_paths(
                repository,
                vec![s("diff-files"), s("--name-only"), s("-z"), s("--"), literal_pathspec(scope)],
                index
            ),
            self.list_paths(repository, others, index),
        )?;
        Ok(Changes { tracked, untracked })
    }

    async fn list_paths(
        &self,
        repository: &Repository,
        args: Vec<String>,
        index: Option<&Path>,
    ) -> Result<Vec<String>, OperationError> {
        let output =
            self.repository_operation(Operation::Refresh, repository, args, RepoOptions { index, ..Default::default() }).await?;
        // Embedded repositories are listed as `dir/`; update-index records them as gitlinks.
        Ok(nuls(&output.text).into_iter().map(|file| file.strip_suffix('/').map(str::to_string).unwrap_or(file)).collect())
    }

    async fn update_index(
        &self,
        repository: &Repository,
        changes: &Changes,
        ignores: Option<&Repository>,
        maximum_untracked_file_bytes: Option<u64>,
        index: Option<&Path>,
        excluded: Option<&HashSet<String>>,
    ) -> Result<Vec<String>, OperationError> {
        let candidates: Vec<String> = changes.tracked.iter().chain(changes.untracked.iter()).cloned().collect();
        if candidates.is_empty() {
            return Ok(Vec::new());
        }
        let computed;
        let excluded = match excluded {
            Some(excluded) => excluded,
            None => {
                computed = match ignores {
                    Some(ignores) => self.ignored(ignores, &candidates).await?,
                    None => HashSet::new(),
                };
                &computed
            }
        };
        let skipped: Vec<String> = match maximum_untracked_file_bytes.filter(|max| *max > 0) {
            Some(maximum) => {
                let checks: Vec<_> = changes
                    .untracked
                    .iter()
                    .filter(|item| !excluded.contains(*item))
                    .map(|item| oversized(repository.worktree.join(item), item.clone(), maximum))
                    .collect();
                futures::stream::iter(checks).buffered(8).filter_map(futures::future::ready).collect().await
            }
            None => Vec::new(),
        };
        let skip: HashSet<&String> = skipped.iter().collect();
        let added: Vec<&String> =
            candidates.iter().filter(|item| !excluded.contains(*item) && !skip.contains(item)).collect();
        // Untracked paths are not in the index, so only tracked paths that are now ignored need removing.
        let removed: Vec<&String> = changes.tracked.iter().filter(|item| excluded.contains(*item)).collect();
        if !removed.is_empty() {
            let stdin = removed.iter().map(|s| s.as_str()).collect::<Vec<_>>().join("\0") + "\0";
            self.repository_operation(
                Operation::Refresh,
                repository,
                vec![s("update-index"), s("--force-remove"), s("-z"), s("--stdin")],
                RepoOptions { stdin: Some(stdin.into_bytes()), index, ..Default::default() },
            )
            .await?;
        }
        // update-index drops a tracked file replaced by an embedded repository; a second line for the
        // same path, in the same call, records its gitlink.
        let checks: Vec<_> = changes
            .tracked
            .iter()
            .filter(|item| !excluded.contains(*item))
            .map(|item| embedded_repository(repository.worktree.join(item).join(".git"), item.clone()))
            .collect();
        let embedded: Vec<String> =
            futures::stream::iter(checks).buffered(8).filter_map(futures::future::ready).collect().await;
        if !added.is_empty() {
            let stdin = added
                .iter()
                .map(|s| s.as_str())
                .chain(embedded.iter().map(String::as_str))
                .collect::<Vec<_>>()
                .join("\0")
                + "\0";
            retry_concurrent_object_write(cfg!(windows), || {
                self.repository_operation(
                    Operation::Refresh,
                    repository,
                    vec![s("update-index"), s("--add"), s("--remove"), s("--replace"), s("-z"), s("--stdin")],
                    RepoOptions { stdin: Some(stdin.clone().into_bytes()), index, ..Default::default() },
                )
            })
            .await?;
        }
        Ok(skipped)
    }

    /// `index.refresh`: refresh only the requested project-relative scope, preserving all other entries.
    pub async fn refresh(
        &self,
        repository: &Repository,
        scope: &str,
        ignores: Option<&Repository>,
        maximum_untracked_file_bytes: Option<u64>,
    ) -> Result<RefreshResult, OperationError> {
        let changes = self.list_changes(repository, scope, None, None).await?;
        let skipped =
            self.update_index(repository, &changes, ignores, maximum_untracked_file_bytes, None, None).await?;
        Ok(RefreshResult { skipped })
    }

    /// `index.ignored`: `check-ignore --no-index --stdin -z` (exit 0 or 1 both succeed).
    pub async fn ignored(&self, repository: &Repository, paths: &[String]) -> Result<HashSet<String>, OperationError> {
        if paths.is_empty() {
            return Ok(HashSet::new());
        }
        let stdin = paths.join("\0") + "\0";
        let result = process::run(
            &repository.worktree,
            &repository_args(repository, vec![s("check-ignore"), s("--no-index"), s("--stdin"), s("-z")]),
            RunOptions { stdin: Some(stdin.into_bytes()), ..Default::default() },
        )
        .await
        .map_err(|cause| OperationError::caused(Operation::ListFiles, &repository.worktree, cause.to_string(), &cause))?;
        if result.exit_code != 0 && result.exit_code != 1 {
            let stderr = result.stderr_text();
            let stderr = stderr.trim();
            return Err(OperationError::new(
                Operation::ListFiles,
                Some(&repository.worktree),
                if stderr.is_empty() { "Failed to check ignored paths" } else { stderr },
            ));
        }
        Ok(nuls(&result.text()).into_iter().collect())
    }

    async fn write_tree_with(&self, repository: &Repository, index: Option<&Path>) -> Result<TreeId, OperationError> {
        let output = retry_concurrent_object_write(cfg!(windows), || {
            self.repository_operation(
                Operation::WriteTree,
                repository,
                vec![s("write-tree")],
                RepoOptions { index, ..Default::default() },
            )
        })
        .await?;
        let tree = output.text.trim().to_string();
        if TREE_ID.is_match(&tree) {
            return Ok(TreeId(tree));
        }
        Err(OperationError::new(Operation::WriteTree, Some(&repository.worktree), format!("Invalid tree ID: {tree}")))
    }

    /// `tree.write`: `write-tree` against the store's own index.
    pub async fn write_tree(&self, repository: &Repository) -> Result<TreeId, OperationError> {
        self.write_tree_with(repository, None).await
    }

    fn temporary_index(repository: &Repository) -> PathBuf {
        repository.git_directory.join(format!("{TEMPORARY_INDEX_PREFIX}{}-{}", now_ms(), unique_suffix()))
    }

    /// Run index writes against a temporary index, then rename it over the store's index.
    /// Returns the value and, when the rename succeeded, the fingerprint of the installed index.
    async fn with_temporary_index<T, F, Fut>(
        &self,
        repository: &Repository,
        use_index: F,
    ) -> Result<(T, Option<String>), OperationError>
    where
        F: FnOnce(PathBuf) -> Fut,
        Fut: Future<Output = Result<T, OperationError>>,
    {
        let current = repository.git_directory.join("index");
        let index = Self::temporary_index(repository);
        let _guard = TempGuard(index.clone());
        if exists(&current).await && std::fs::hard_link(&current, &index).is_err() {
            copy_index(&current, &index).map_err(|cause| {
                OperationError::caused(
                    Operation::Refresh,
                    &repository.git_directory,
                    "Failed to prepare a temporary index",
                    cause,
                )
            })?;
        }
        let value = use_index(index.clone()).await?;
        // Fingerprint before the rename, so a concurrent writer's index can never be paired with this tree.
        let fingerprint = index_fingerprint(&index);
        let mut renamed = false;
        for attempt in 0..4 {
            if attempt > 0 {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
            if std::fs::rename(&index, &current).is_ok() {
                renamed = true;
                break;
            }
        }
        Ok((value, if renamed { fingerprint } else { None }))
    }

    /// Changes are listed against the temporary index they are written to, never the store's index.
    async fn attempt_capture(&self, input: &CaptureInput) -> Result<TreeId, OperationError> {
        let repository = &input.repository;
        let source_exclude = input.ignores.as_ref().map(|ignores| ignores.common_directory.join("info").join("exclude"));
        let exclude_from = match source_exclude {
            Some(file) if exists(&file).await => Some(file),
            _ => None,
        };
        let exclude_from = exclude_from.as_deref();
        let (tree, installed) = self
            .with_temporary_index(repository, |index| async move {
                let scanned = index_fingerprint(&index);
                let listings: Vec<_> = input
                    .scopes
                    .iter()
                    .map(|scope| self.list_changes(repository, scope, Some(&index), exclude_from))
                    .collect();
                let changes = futures::future::try_join_all(listings).await?;
                let last = self.inner.last_captures.lock().get(&repository.git_directory).cloned();
                let untracked: Vec<String> = changes.iter().flat_map(|c| c.untracked.iter().cloned()).collect();
                let oversized = self.inner.oversized.lock().get(&repository.git_directory).cloned();
                let still_oversized: HashSet<String> = match &oversized {
                    Some(oversized) => {
                        let checks: Vec<_> = untracked
                            .iter()
                            .filter(|file| oversized.contains_key(*file))
                            .map(|file| async move {
                                let current = oversized_stamp(input, file).await;
                                (current.is_some() && current.as_ref() == oversized.get(file)).then(|| file.clone())
                            })
                            .collect();
                        futures::stream::iter(checks).buffered(8).filter_map(futures::future::ready).collect().await
                    }
                    None => HashSet::new(),
                };
                // Paths the source ignores are listed as untracked on every scan; they leave the index unchanged.
                let excluded = match (&scanned, &last) {
                    (Some(scanned), Some(last))
                        if &last.fingerprint == scanned && changes.iter().all(|c| c.tracked.is_empty()) =>
                    {
                        Some(match &input.ignores {
                            Some(ignores) => {
                                let paths: Vec<String> =
                                    untracked.iter().filter(|file| !still_oversized.contains(*file)).cloned().collect();
                                self.ignored(ignores, &paths).await?
                            }
                            None => HashSet::new(),
                        })
                    }
                    _ => None,
                };
                if let (Some(last), Some(excluded)) = (&last, &excluded)
                    && untracked.iter().all(|file| excluded.contains(file) || still_oversized.contains(file))
                {
                    return Ok(last.tree.clone());
                }
                let mut skipped = Vec::new();
                for change in &changes {
                    skipped.extend(
                        self.update_index(
                            repository,
                            change,
                            input.ignores.as_ref(),
                            input.maximum_untracked_file_bytes,
                            Some(&index),
                            excluded.as_ref(),
                        )
                        .await?,
                    );
                }
                let stamps: Vec<_> = skipped
                    .iter()
                    .map(|file| async move { oversized_stamp(input, file).await.map(|stamp| (file.clone(), stamp)) })
                    .collect();
                let stamps: HashMap<String, String> =
                    futures::stream::iter(stamps).buffered(8).filter_map(futures::future::ready).collect().await;
                self.inner.oversized.lock().insert(repository.git_directory.clone(), stamps);
                self.write_tree_with(repository, Some(&index)).await
            })
            .await?;
        let mut last = self.inner.last_captures.lock();
        match installed {
            Some(fingerprint) => {
                last.insert(repository.git_directory.clone(), LastCapture { fingerprint, tree: tree.clone() });
            }
            None => {
                last.remove(&repository.git_directory);
            }
        }
        Ok(tree)
    }

    /// `tree.capture`: record the scoped worktree state as a tree object. Callers queued behind a
    /// running capture share the next one, which starts after they all asked, so it still sees
    /// every change made before each call.
    pub async fn capture(&self, input: &CaptureInput) -> Result<TreeId, OperationError> {
        let key = serde_json::to_string(&(
            p(&input.repository.git_directory),
            p(&input.repository.worktree),
            &input.scopes,
            input.ignores.as_ref().map(|ignores| p(&ignores.git_directory)),
            input.maximum_untracked_file_bytes,
        ))
        .unwrap_or_default();
        let pending = self.inner.pending_captures.lock().get(&key).map(|entry| entry.receiver.clone());
        if let Some(mut receiver) = pending
            && let Ok(value) = receiver.wait_for(Option::is_some).await
            && let Some(result) = value.clone()
        {
            return result;
        }
        // The leader was cancelled before it finished: run a capture of our own.
        let id = self.inner.pending_ids.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let (sender, receiver) = tokio::sync::watch::channel(None);
        self.inner.pending_captures.lock().insert(key.clone(), PendingCapture { id, receiver });
        let guard = PendingGuard { inner: self.inner.clone(), key, id };
        let result = self.capture_locked(input, &guard).await;
        guard.release();
        let _ = sender.send(Some(result.clone()));
        result
    }

    async fn capture_locked(&self, input: &CaptureInput, pending: &PendingGuard) -> Result<TreeId, OperationError> {
        let lock = self.lock_for(&input.repository);
        let _held = lock.lock().await;
        pending.release();
        let first = self.inner.prepared_stores.lock().insert(input.repository.git_directory.clone());
        if first {
            self.sweep_temporary_indexes(&input.repository).await;
            self.ensure_snapshot_config(&input.repository).await;
        }
        match self.attempt_capture(input).await {
            Ok(tree) => Ok(tree),
            Err(error) => {
                if !self.is_index_unreadable(&input.repository).await {
                    return Err(error);
                }
                tracing::warn!(
                    directory = %input.repository.git_directory.display(),
                    message = %error.message,
                    "rebuilding unreadable snapshot index"
                );
                self.rebuild_index(&input.repository, input.ignores.as_ref()).await;
                self.attempt_capture(input).await
            }
        }
    }

    /// Only an index Git itself cannot read is rebuilt, decided by exit status.
    async fn is_index_unreadable(&self, repository: &Repository) -> bool {
        if !exists(&repository.git_directory.join("index")).await {
            return false;
        }
        match self
            .repository_operation(
                Operation::Refresh,
                repository,
                vec![s("ls-files"), s("-z"), s("--"), literal_pathspec(".opencode-probe")],
                RepoOptions::default(),
            )
            .await
        {
            Ok(_) => false,
            Err(error) => error.cause.is_none(),
        }
    }

    async fn rebuild_index(&self, repository: &Repository, source: Option<&Repository>) {
        self.inner.last_captures.lock().remove(&repository.git_directory);
        let temporary = Self::temporary_index(repository);
        let _guard = TempGuard(temporary.clone());
        // The source's stat cache and cache-tree spare recovery from rehashing every tracked file.
        let copied = match source {
            Some(source) => self.seed_index(repository, source, &temporary).await.is_ok(),
            None => false,
        };
        let index = repository.git_directory.join("index");
        if copied {
            let _ = std::fs::rename(&temporary, &index);
        } else {
            let _ = std::fs::remove_file(&index);
        }
    }

    /// Stores created by earlier releases would otherwise keep their original settings.
    async fn ensure_snapshot_config(&self, repository: &Repository) {
        let file = repository.git_directory.join(SNAPSHOT_CONFIG_FILE);
        let Ok(current) = tokio::fs::read_to_string(&file).await else { return };
        if current == SNAPSHOT_CONFIG {
            return;
        }
        // Concurrent Git processes must never read a partially written file.
        let temporary = PathBuf::from(format!("{}.{}", file.display(), unique_suffix()));
        let _guard = TempGuard(temporary.clone());
        if tokio::fs::write(&temporary, SNAPSHOT_CONFIG).await.is_ok() {
            let _ = tokio::fs::rename(&temporary, &file).await;
        }
    }

    /// Temporary indexes left by killed processes; age comes from the creation time in the name.
    async fn sweep_temporary_indexes(&self, repository: &Repository) {
        let cutoff = now_ms().saturating_sub(HOUR_MS);
        let Ok(entries) = std::fs::read_dir(&repository.git_directory) else { return };
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            let Some(rest) = name.strip_prefix(TEMPORARY_INDEX_PREFIX) else { continue };
            // `Number("")` is 0 and `Number("abc")` is NaN (never < cutoff).
            let first = rest.split('-').next().unwrap_or("");
            let created = if first.trim().is_empty() { Some(0u128) } else { first.trim().parse::<u128>().ok() };
            if created.is_some_and(|created| created < cutoff) {
                remove_file_force(&entry.path()).await;
            }
        }
    }

    // ---------------------------------------------------------------- trees

    /// `tree.files`: both sides of renames (`--no-renames`).
    pub async fn tree_files(
        &self,
        repository: &Repository,
        from: &TreeId,
        to: &TreeId,
    ) -> Result<Vec<String>, OperationError> {
        let output = self
            .repository_operation(
                Operation::ListFiles,
                repository,
                vec![s("diff"), s("--name-only"), s("--no-renames"), s("-z"), from.0.clone(), to.0.clone()],
                RepoOptions::default(),
            )
            .await?;
        Ok(nuls(&output.text))
    }

    /// `tree.diff`: structured per-file diffs with a 10 MB total patch budget.
    pub async fn tree_diff(&self, repository: &Repository, input: &DiffInput) -> Result<Vec<FileDiff>, OperationError> {
        if input.paths.as_ref().is_some_and(|paths| paths.is_empty()) {
            return Ok(Vec::new());
        }
        let groups: Vec<Option<Vec<String>>> = match &input.paths {
            Some(paths) => path_groups(paths).into_iter().map(Some).collect(),
            None => vec![None],
        };
        let mut diffs = Vec::new();
        let mut budget = patch::MAX_TOTAL_PATCH_BYTES as i64;
        for paths in groups {
            let (group, bytes, truncated) = self.diff_group(repository, input, paths.as_deref(), budget).await?;
            diffs.extend(group);
            budget = if truncated { 0 } else { budget - bytes as i64 };
        }
        Ok(diffs)
    }

    async fn diff_group(
        &self,
        repository: &Repository,
        input: &DiffInput,
        paths: Option<&[String]>,
        budget: i64,
    ) -> Result<(Vec<FileDiff>, usize, bool), OperationError> {
        let mut args = vec![s("--no-renames"), input.from.0.clone(), input.to.0.clone(), s("--")];
        args.extend(paths.unwrap_or(&[]).iter().cloned());
        let with = |prefix: &[&str]| -> Vec<String> {
            prefix.iter().map(|v| s(v)).chain(args.iter().cloned()).collect()
        };
        let names = self.repository_operation(
            Operation::Diff,
            repository,
            with(&["--literal-pathspecs", "diff", "--name-status", "-z"]),
            RepoOptions::default(),
        );
        let numbers = self.repository_operation(
            Operation::Diff,
            repository,
            with(&["--literal-pathspecs", "diff", "--numstat", "-z"]),
            RepoOptions::default(),
        );
        let unified = format!("--unified={}", input.context.unwrap_or(3));
        let patch_args = with(&["--literal-pathspecs", "-c", "core.quotepath=false", "diff", "--no-ext-diff", &unified]);
        // Patch headers have no -z form: unquoted paths keep chunks matching non-ASCII names.
        let patch_future = async {
            if budget > 0 {
                self.repository_operation(
                    Operation::Diff,
                    repository,
                    patch_args,
                    RepoOptions { max_output_bytes: Some(budget as usize), ..Default::default() },
                )
                .await
            } else {
                Ok(OpOutput { text: String::new(), truncated: true })
            }
        };
        let (names, numbers, patch_output) = tokio::try_join!(names, numbers, patch_future)?;
        let statuses = nuls(&names.text);
        let mut files: Vec<(String, FileStatus)> = Vec::new();
        for (index, code) in statuses.iter().enumerate() {
            if index % 2 != 0 {
                continue;
            }
            let Some(file) = statuses.get(index + 1) else { continue };
            let status = if code.starts_with('A') {
                FileStatus::Added
            } else if code.starts_with('D') {
                FileStatus::Deleted
            } else {
                FileStatus::Modified
            };
            files.push((file.clone(), status));
        }
        let mut stats: HashMap<String, (bool, u64, u64)> = HashMap::new();
        for line in nuls(&numbers.text) {
            let parts: Vec<&str> = line.split('\t').collect();
            if parts.len() < 3 || parts[0].is_empty() || parts[1].is_empty() {
                continue;
            }
            let file = parts[2..].join("\t");
            let value = if parts[0] == "-" || parts[1] == "-" {
                (true, 0, 0)
            } else {
                (false, js_number(parts[0]), js_number(parts[1]))
            };
            stats.insert(file, value);
        }
        let patch = Patch { text: patch_output.text, truncated: patch_output.truncated };
        let patches = patch::chunks_by_file(&patch, |index| files.get(index).map(|f| f.0.clone()));
        let diffs = files
            .into_iter()
            .map(|(file, status)| {
                let stat = stats.get(&file);
                let patch_text = if stat.is_some_and(|s| s.0) {
                    String::new()
                } else {
                    patches.get(&file).cloned().unwrap_or_else(|| patch::empty_patch(&file))
                };
                FileDiff {
                    additions: stat.map(|s| s.1).unwrap_or(0),
                    deletions: stat.map(|s| s.2).unwrap_or(0),
                    patch: patch_text,
                    status,
                    file,
                    more: None,
                }
            })
            .collect();
        Ok((diffs, patch.text.len(), patch.truncated))
    }

    async fn has_entry(&self, repository: &Repository, tree: &TreeId, file: &str) -> Result<bool, OperationError> {
        let output = self
            .repository_operation(
                Operation::Restore,
                repository,
                vec![s("ls-tree"), s("-z"), tree.0.clone(), s("--"), literal_pathspec(file)],
                RepoOptions::default(),
            )
            .await?;
        let text = output.text.strip_suffix('\0').unwrap_or(&output.text);
        if text.is_empty() {
            return Ok(false);
        }
        if !TREE_ENTRY_PREFIX.is_match(text) {
            return Err(OperationError::new(
                Operation::Restore,
                Some(&repository.worktree),
                format!("Invalid tree entry for {file}"),
            ));
        }
        Ok(true)
    }

    async fn remove_path(&self, repository: &Repository, file: &str) -> Result<(), OperationError> {
        remove_recursive(&repository.worktree.join(file)).await.map_err(|cause| {
            OperationError::caused(Operation::Restore, &repository.worktree, format!("Failed to remove {file}"), cause)
        })
    }

    async fn paths_in_tree(
        &self,
        repository: &Repository,
        tree: &TreeId,
        files: &[String],
    ) -> Result<HashSet<String>, OperationError> {
        let pathspecs: Vec<String> = files.iter().map(|f| literal_pathspec(f)).collect();
        let fits = pathspecs.iter().map(|p| js_len(p) + 1).sum::<usize>() <= ARGUMENT_BUDGET;
        let args = if fits {
            let mut args = vec![s("ls-tree"), s("-z"), tree.0.clone(), s("--")];
            args.extend(pathspecs);
            args
        } else {
            vec![s("ls-tree"), s("-r"), s("-t"), s("-z"), s("--full-tree"), tree.0.clone()]
        };
        let output = self.repository_operation(Operation::Restore, repository, args, RepoOptions::default()).await?;
        let mut listed = HashSet::new();
        for record in nuls(&output.text) {
            match TREE_ENTRY.captures(&record) {
                Some(captures) => {
                    listed.insert(captures[1].to_string());
                }
                None => {
                    return Err(OperationError::new(
                        Operation::Restore,
                        Some(&repository.worktree),
                        format!("Invalid tree entry: {record}"),
                    ));
                }
            }
        }
        Ok(listed)
    }

    /// `tree.restore`: restore each path from its tree; paths absent from the tree are deleted.
    /// `files` keeps insertion order like a JS `Map`.
    pub async fn restore(&self, repository: &Repository, files: &[(String, TreeId)]) -> Result<(), OperationError> {
        let lock = self.lock_for(repository);
        let _held = lock.lock().await;
        if files.is_empty() {
            return Ok(());
        }
        let keys: Vec<String> = files.iter().map(|(file, _)| file.clone()).collect();
        if !can_batch_restore(&keys) {
            for (file, tree) in files {
                if !self.has_entry(repository, tree, file).await? {
                    self.remove_path(repository, file).await?;
                    continue;
                }
                self.with_temporary_index(repository, |index| async move {
                    self.repository_operation(
                        Operation::Restore,
                        repository,
                        vec![s("checkout"), tree.0.clone(), s("--"), literal_pathspec(file)],
                        RepoOptions { index: Some(&index), ..Default::default() },
                    )
                    .await
                    .map(|_| ())
                })
                .await?;
            }
            return Ok(());
        }
        let mut groups: Vec<(TreeId, Vec<String>)> = Vec::new();
        for (file, tree) in files {
            match groups.iter_mut().find(|(t, _)| t == tree) {
                Some((_, list)) => list.push(file.clone()),
                None => groups.push((tree.clone(), vec![file.clone()])),
            }
        }
        let mut plan: Vec<(TreeId, Vec<String>, Vec<String>)> = Vec::new();
        for (tree, files) in groups {
            let present = self.paths_in_tree(repository, &tree, &files).await?;
            let (inside, absent): (Vec<String>, Vec<String>) = files.into_iter().partition(|f| present.contains(f));
            plan.push((tree, inside, absent));
        }
        // Checkouts go first, so one failing removal cannot stop the other files from being restored.
        let checkouts: Vec<&(TreeId, Vec<String>, Vec<String>)> =
            plan.iter().filter(|item| !item.1.is_empty()).collect();
        if !checkouts.is_empty() {
            self.with_temporary_index(repository, |index| async move {
                for (tree, present, _) in checkouts {
                    let stdin = present.iter().map(|f| literal_pathspec(f)).collect::<Vec<_>>().join("\0") + "\0";
                    self.repository_operation(
                        Operation::Restore,
                        repository,
                        vec![s("checkout"), tree.0.clone(), s("--pathspec-from-file=-"), s("--pathspec-file-nul")],
                        RepoOptions { stdin: Some(stdin.into_bytes()), index: Some(&index), ..Default::default() },
                    )
                    .await?;
                }
                Ok(())
            })
            .await?;
        }
        let absent: Vec<&String> = plan.iter().flat_map(|item| item.2.iter()).collect();
        let removals: Vec<_> = absent.into_iter().map(|file| self.remove_path(repository, file)).collect();
        let results: Vec<Result<(), OperationError>> = futures::stream::iter(removals).buffered(16).collect().await;
        results.into_iter().collect()
    }

    // ---------------------------------------------------------------- objects

    /// `objects.pack`: pack loose objects and combine small packs without dropping any object.
    pub async fn pack(&self, repository: &Repository) -> Result<(), OperationError> {
        let objects = repository.git_directory.join("objects");
        let pack_directory = objects.join("pack");
        let loose = loose_objects(&objects);
        let packs = local_packs(&pack_directory);
        let combine = packs.len() >= PACK_COUNT_LIMIT;
        if loose.len() < LOOSE_OBJECT_LIMIT && !combine {
            return Ok(());
        }
        let lock = repository.git_directory.join("opencode-pack.lock");
        let stale = std::fs::metadata(&lock).map(|m| mtime_ms(&m) < now_ms().saturating_sub(HOUR_MS)).unwrap_or(false);
        if stale {
            let _ = std::fs::remove_file(&lock);
        }
        let acquired = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&lock)
            .and_then(|mut f| {
                use std::io::Write;
                f.write_all(std::process::id().to_string().as_bytes())
            })
            .is_ok();
        if !acquired {
            return Ok(());
        }
        let _guard = TempGuard(lock);
        sweep_temporary_packs(&pack_directory);
        let mut combined: Vec<(String, Vec<String>)> = Vec::new();
        if combine {
            for pack in &packs {
                let oids = self.pack_index(repository, &pack_directory.join(format!("{pack}.idx"))).await?;
                combined.push((pack.clone(), oids));
            }
        }
        let mut seen = HashSet::new();
        let object_ids: Vec<String> = loose
            .iter()
            .map(|(oid, _)| oid.clone())
            .chain(combined.iter().flat_map(|(_, oids)| oids.iter().cloned()))
            .filter(|oid| seen.insert(oid.clone()))
            .collect();
        if object_ids.is_empty() {
            return Ok(());
        }
        let output = self
            .repository_operation(
                Operation::Pack,
                repository,
                vec![
                    s("-c"),
                    s("pack.threads=2"),
                    s("-c"),
                    s("pack.windowMemory=64m"),
                    s("pack-objects"),
                    s("-q"),
                    s("--non-empty"),
                    p(&pack_directory.join("pack")),
                ],
                RepoOptions { stdin: Some((object_ids.join("\n") + "\n").into_bytes()), ..Default::default() },
            )
            .await?;
        let new_packs: Vec<String> =
            output.text.split('\n').map(|l| l.trim().to_string()).filter(|l| !l.is_empty()).collect();
        let mut packed = HashSet::new();
        for name in &new_packs {
            packed.extend(self.pack_index(repository, &pack_directory.join(format!("pack-{name}.idx"))).await?);
        }
        let missing = object_ids.iter().filter(|oid| !packed.contains(*oid)).count();
        if missing > 0 {
            return Err(OperationError::new(
                Operation::Pack,
                Some(&repository.git_directory),
                format!("Packed {} objects but {} are missing; nothing was removed", packed.len(), missing),
            ));
        }
        // Deletion is quick and must not stop halfway.
        for (_, file) in &loose {
            let _ = std::fs::remove_file(file);
        }
        let new_names: HashSet<String> = new_packs.iter().map(|name| format!("pack-{name}")).collect();
        for (pack, _) in combined.iter().filter(|(pack, _)| !new_names.contains(pack)) {
            // The index goes first so readers never see an index without its pack.
            for extension in [".idx", ".pack", ".rev", ".bitmap", ".mtimes"] {
                let _ = std::fs::remove_file(pack_directory.join(format!("{pack}{extension}")));
            }
        }
        Ok(())
    }

    async fn pack_index(&self, repository: &Repository, file: &Path) -> Result<Vec<String>, OperationError> {
        let bytes = tokio::fs::read(file).await.map_err(|cause| {
            OperationError::caused(
                Operation::Pack,
                &repository.git_directory,
                format!("Failed to read {}", file.display()),
                cause,
            )
        })?;
        if let Some(parsed) = pack_index_object_ids(&bytes) {
            return Ok(parsed);
        }
        let output = self
            .repository_operation(
                Operation::Pack,
                repository,
                vec![s("show-index")],
                RepoOptions { stdin: Some(bytes), ..Default::default() },
            )
            .await?;
        Ok(output.text.split('\n').filter_map(|line| line.split(' ').nth(1).map(str::to_string)).collect())
    }

    // ---------------------------------------------------------------- worktrees

    async fn worktree_run(
        &self,
        operation: WorktreeOperation,
        args: Vec<String>,
        directory: Option<&Path>,
        cwd: &Path,
    ) -> Result<String, WorktreeError> {
        let result = execute(cwd, args).await.map_err(|cause| WorktreeError {
            operation,
            message: cause.to_string(),
            directory: directory.map(Path::to_path_buf),
            force_required: None,
            cause: Some(cause.to_string()),
        })?;
        if result.exit_code == 0 {
            return Ok(result.text());
        }
        let stderr = result.stderr_text();
        let stdout = result.text();
        let message = [stderr.trim(), stdout.trim()]
            .into_iter()
            .find(|v| !v.is_empty())
            .unwrap_or("Git failed")
            .to_string();
        Err(WorktreeError {
            operation,
            force_required: Some(operation == WorktreeOperation::Remove && FORCE_REQUIRED.is_match(&message)),
            message,
            directory: directory.map(Path::to_path_buf),
            cause: None,
        })
    }

    /// `worktree.create`: `git worktree add --detach -- <dir> <ref or HEAD>`.
    pub async fn worktree_create(
        &self,
        repository: &Repository,
        directory: &Path,
        reference: Option<&str>,
    ) -> Result<Repository, WorktreeError> {
        self.worktree_run(
            WorktreeOperation::Create,
            vec![s("worktree"), s("add"), s("--detach"), s("--"), p(directory), s(reference.unwrap_or("HEAD"))],
            Some(directory),
            &repository.worktree,
        )
        .await?;
        match self.discover(directory).await {
            Some(repository) => Ok(repository),
            None => Err(WorktreeError {
                operation: WorktreeOperation::Create,
                message: s("Created worktree could not be opened"),
                directory: Some(directory.to_path_buf()),
                force_required: None,
                cause: None,
            }),
        }
    }

    /// `worktree.remove`: `git worktree remove [--force] <dir>`, run from the common directory.
    pub async fn worktree_remove(
        &self,
        repository: &Repository,
        directory: &Path,
        force: bool,
    ) -> Result<(), WorktreeError> {
        let mut args = vec![s("worktree"), s("remove")];
        if force {
            args.push(s("--force"));
        }
        args.push(p(directory));
        self.worktree_run(WorktreeOperation::Remove, args, Some(directory), &repository.common_directory).await?;
        Ok(())
    }

    /// `worktree.list`: `git worktree list --porcelain`; the first entry is `main`.
    pub async fn worktree_list(&self, repository: &Repository) -> Result<Vec<Worktree>, WorktreeError> {
        let text = self
            .worktree_run(
                WorktreeOperation::List,
                vec![s("worktree"), s("list"), s("--porcelain")],
                None,
                &repository.worktree,
            )
            .await?;
        Ok(text
            .split('\n')
            .filter(|line| line.starts_with("worktree "))
            .enumerate()
            .map(|(index, line)| Worktree {
                directory: resolve_git_path(&repository.worktree, line["worktree ".len()..].trim()),
                kind: if index == 0 { WorktreeKind::Main } else { WorktreeKind::Linked },
            })
            .collect())
    }
}

/// `vcs.ts` branch-metadata filter for git: a file named `HEAD` inside the (realpath'd) common dir.
pub fn is_branch_metadata(store: &Path, file: &Path) -> bool {
    file.file_name().is_some_and(|name| name == "HEAD") && crate::paths::contains(store, file)
}

async fn oversized(path: PathBuf, item: String, maximum: u64) -> Option<String> {
    match tokio::fs::metadata(path).await {
        Ok(info) if info.is_file() && info.len() > maximum => Some(item),
        _ => None,
    }
}

/// `oversizedStamp`: `<size>:<mtime ms>` for an untracked file still over the capture's size limit.
async fn oversized_stamp(input: &CaptureInput, file: &str) -> Option<String> {
    let maximum = input.maximum_untracked_file_bytes.filter(|max| *max > 0)?;
    let info = tokio::fs::metadata(input.repository.worktree.join(file)).await.ok()?;
    (info.is_file() && info.len() > maximum).then(|| format!("{}:{}", info.len(), mtime_ms(&info)))
}

async fn embedded_repository(dotgit: PathBuf, item: String) -> Option<String> {
    if exists(&dotgit).await { Some(item) } else { None }
}

/// `Number(text)` for git's numstat columns.
fn js_number(text: &str) -> u64 {
    text.trim().parse::<u64>().unwrap_or(0)
}

fn failure_message(result: &RunResult, operation: Operation) -> String {
    let stderr = result.stderr_text();
    let stdout = result.text();
    [stderr.trim(), stdout.trim()]
        .into_iter()
        .find(|v| !v.is_empty())
        .map(str::to_string)
        .unwrap_or_else(|| format!("Git {} failed", operation.as_str()))
}

fn loose_objects(objects: &Path) -> Vec<(String, PathBuf)> {
    let Ok(entries) = std::fs::read_dir(objects) else { return Vec::new() };
    let mut prefixes: Vec<String> = entries
        .flatten()
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|name| FANOUT.is_match(name))
        .collect();
    prefixes.sort();
    let mut out = Vec::new();
    for prefix in prefixes {
        let Ok(entries) = std::fs::read_dir(objects.join(&prefix)) else { continue };
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            if LOOSE.is_match(&name) {
                out.push((format!("{prefix}{name}"), entry.path()));
            }
        }
    }
    out
}

/// Packs with a `.keep` marker or a promisor file belong to someone else's policy and are left alone.
fn local_packs(directory: &Path) -> Vec<String> {
    let Ok(entries) = std::fs::read_dir(directory) else { return Vec::new() };
    let names: HashSet<String> = entries.flatten().map(|e| e.file_name().to_string_lossy().into_owned()).collect();
    let mut packs: Vec<String> = names
        .iter()
        .filter(|entry| entry.starts_with("pack-") && entry.ends_with(".pack"))
        .map(|entry| entry[..entry.len() - ".pack".len()].to_string())
        .filter(|pack| {
            names.contains(&format!("{pack}.idx"))
                && !names.contains(&format!("{pack}.keep"))
                && !names.contains(&format!("{pack}.promisor"))
        })
        .collect();
    packs.sort();
    packs
}

/// `pack-objects` leftovers (`tmp_*`, packs without an index) older than one hour.
fn sweep_temporary_packs(directory: &Path) {
    let cutoff = now_ms().saturating_sub(HOUR_MS);
    let Ok(entries) = std::fs::read_dir(directory) else { return };
    let names: Vec<String> = entries.flatten().map(|e| e.file_name().to_string_lossy().into_owned()).collect();
    let indexed: HashSet<&str> =
        names.iter().filter(|n| n.ends_with(".idx")).map(|n| &n[..n.len() - 4]).collect();
    for name in &names {
        let candidate = name.starts_with("tmp_")
            || (name.starts_with("pack-") && name.ends_with(".pack") && !indexed.contains(&name[..name.len() - 5]));
        if !candidate {
            continue;
        }
        let path = directory.join(name);
        if let Ok(meta) = std::fs::metadata(&path)
            && mtime_ms(&meta) < cutoff {
                let _ = std::fs::remove_file(&path);
            }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn assert_send<T: Send>(_: &T) {}

    #[test]
    fn service_futures_are_send() {
        let git = Git::new();
        let repo = Repository { worktree: "/x".into(), git_directory: "/y".into(), common_directory: "/y".into() };
        let input = CaptureInput { repository: repo.clone(), scopes: vec![], ignores: None, maximum_untracked_file_bytes: None };
        assert_send(&git.capture(&input));
        assert_send(&git.restore(&repo, &[]));
        assert_send(&git.pack(&repo));
        assert_send(&git.tree_diff(&repo, &DiffInput::default()));
        assert_send(&git.refresh(&repo, ".", None, None));
    }

    #[test]
    fn batch_restore_rules() {
        let v = |items: &[&str]| items.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        assert!(can_batch_restore(&v(&["a.txt", "dir/b.txt"])));
        assert!(!can_batch_restore(&v(&["dir", "dir/b.txt"])));
        assert!(!can_batch_restore(&v(&["A.txt", "a.txt"])));
        assert!(!can_batch_restore(&v(&["caf\u{e9}.txt"])));
        assert!(!can_batch_restore(&v(&["trailing."])));
        assert!(!can_batch_restore(&v(&["a:b"])));
    }

    #[test]
    fn groups_sorted_and_budgeted() {
        let paths: Vec<String> = vec!["b".into(), "a".into(), "c".into()];
        assert_eq!(path_groups(&paths), vec![vec!["a".to_string(), "b".into(), "c".into()]]);
        let many: Vec<String> = (0..30_000).map(|i| format!("missing/{i:06}/{}.txt", "x".repeat(80))).collect();
        let groups = path_groups(&many);
        assert!(groups.len() > 1);
        assert!(groups.iter().all(|g| g.iter().map(|p| p.len() + 3).sum::<usize>() <= ARGUMENT_BUDGET));
        assert_eq!(groups.iter().map(Vec::len).sum::<usize>(), 30_000);
    }

    #[test]
    fn parses_v2_pack_index() {
        let count = 2u32;
        let mut bytes = Vec::new();
        bytes.extend(0xff74_4f63u32.to_be_bytes());
        bytes.extend(2u32.to_be_bytes());
        for i in 0..256u32 {
            bytes.extend((if i < 0x11 { 0 } else { count }).to_be_bytes());
        }
        bytes.extend([0x11u8; 20]);
        bytes.extend([0x22u8; 20]);
        bytes.extend(vec![0u8; 8 * count as usize]);
        bytes.extend([0u8; 40]);
        let ids = pack_index_object_ids(&bytes).unwrap();
        assert_eq!(ids, vec!["11".repeat(20), "22".repeat(20)]);
        assert!(pack_index_object_ids(&bytes[..bytes.len() - 1]).is_none());
    }

    #[test]
    fn snapshot_config_is_byte_identical() {
        assert_eq!(
            SNAPSHOT_CONFIG,
            "[core]\n\tautocrlf = false\n\tlongpaths = true\n\tsymlinks = true\n\tfsmonitor = false\n\tuntrackedCache = true\n\t# A split index cannot name its shared file once manyFiles skips index checksums.\n\tsplitIndex = false\n[feature]\n\tmanyFiles = true\n[index]\n\tversion = 4\n\tthreads = true\n"
        );
    }
}

#[cfg(test)]
mod retry_tests {
    use super::*;

    #[tokio::test]
    async fn retries_git_exits_twice_and_only_when_enabled() {
        let failing = |cause: Option<&str>| OperationError {
            operation: Operation::WriteTree,
            message: "fail".into(),
            directory: None,
            cause: cause.map(String::from),
        };
        for (retry, cause, attempts) in [(true, None, 3), (false, None, 1), (true, Some("spawn"), 1)] {
            let mut count = 0;
            let result: Result<(), _> = retry_concurrent_object_write(retry, || {
                count += 1;
                let error = failing(cause);
                async move { Err(error) }
            })
            .await;
            assert!(result.is_err());
            assert_eq!(count, attempts, "retry {retry}, cause {cause:?}");
        }
        let mut count = 0;
        let result = retry_concurrent_object_write(true, || {
            count += 1;
            let ok = count == 2;
            let error = failing(None);
            async move { if ok { Ok(7) } else { Err(error) } }
        })
        .await;
        assert_eq!(result.unwrap(), 7);
    }
}
