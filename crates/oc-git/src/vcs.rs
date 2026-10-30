//! Port of the git VCS provider `packages/core/src/plugin/vcs/git.ts` (`opencode.vcs.git`) plus the
//! provider-independent parts of `packages/core/src/vcs.ts` (diff budget, branch-metadata filter).
//!
//! [`GitVcs`] is the adapter oc-core's `Vcs` service calls: `info`, `base`, `branches`, `status`,
//! `diff`, and `init`. Listing commands run in the Location directory with pathspec `.`; per-file
//! commands run from the worktree root because git lists root-relative paths.

use std::cmp::Ordering;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use once_cell::sync::Lazy;
use regex::Regex;
use serde::{Deserialize, Serialize};

use oc_schema::types::{FileDiff, FileStatus as Kind};

use crate::patch::{self, MAX_PATCH_BYTES, MAX_TOTAL_PATCH_BYTES, PATCH_CONTEXT_LINES, Patch};
use crate::process::{self, RunOptions};

// ---------------------------------------------------------------------------- schema (`schema/src/vcs.ts`)

/// `Vcs.Branch`
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Branch {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub current: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default: Option<String>,
}

/// `Vcs.Info`
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Info {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub provider: Option<String>,
    pub branch: Branch,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum BaseSource {
    Reflog,
    Default,
}

/// `Vcs.Base`
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Base {
    pub name: String,
    #[serde(rename = "ref")]
    pub reference: String,
    pub source: BaseSource,
}

/// `Vcs.Mode`
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Mode {
    Working,
    Branch,
    Committed,
}

/// `Vcs.Untracked`: how a diff lists untracked files, one by one or each untracked folder as one
/// `dir/` entry.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum UntrackedMode {
    #[default]
    Files,
    Folders,
}

impl UntrackedMode {
    pub fn as_str(self) -> &'static str {
        match self {
            UntrackedMode::Files => "files",
            UntrackedMode::Folders => "folders",
        }
    }
}

/// Entries a folder listing shows, and checks per Git call.
pub const MAX_FOLDER_ENTRIES: usize = 500;
/// Entries a folder listing checks before it stops and reports that more were not checked.
pub const MAX_FOLDER_SCAN: usize = 5_000;
/// Time a folder listing spends checking before it stops.
pub const MAX_FOLDER_SCAN_MS: u128 = 1_000;

/// `Vcs.FileStatus`
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VcsFileStatus {
    pub file: String,
    pub additions: u64,
    pub deletions: u64,
    pub status: Kind,
}

/// `Vcs.DiffError`
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{message}")]
pub struct DiffError {
    pub message: String,
}

impl DiffError {
    pub const TAG: &'static str = "Vcs.DiffError";
    fn new(message: impl Into<String>) -> Self {
        DiffError { message: message.into() }
    }
}

/// Failure message of a provider's `init`.
pub const INIT_FAILED_MESSAGE: &str = "Git initialization failed";
/// `Vcs.base` failure message when a provider error is not a `DiffError`.
pub const BASE_FAILED_MESSAGE: &str = "VCS provider could not resolve a review base";
/// `Vcs.diff` failure message when a provider error is not a `DiffError`.
pub const DIFF_FAILED_MESSAGE: &str = "VCS provider could not produce a diff";

#[derive(Debug, Clone, Default)]
pub struct DiffOptions {
    /// Context lines; `None` means [`PATCH_CONTEXT_LINES`] (full file).
    pub context: Option<u64>,
    pub base: Option<String>,
    /// `None` means [`UntrackedMode::Files`].
    pub untracked: Option<UntrackedMode>,
}

#[derive(Debug, Clone, Default)]
pub struct BranchOptions {
    pub search: Option<String>,
    pub limit: Option<u64>,
}

// ---------------------------------------------------------------------------- vcs.ts helpers

/// `Vcs.diff` total budget: once 10 MB of patches have been returned, later files get `emptyPatch`.
pub fn apply_total_patch_budget(rows: Vec<FileDiff>) -> Vec<FileDiff> {
    let mut total = 0usize;
    rows.into_iter()
        .map(|mut row| {
            let bytes = row.patch.len();
            if total + bytes > MAX_TOTAL_PATCH_BYTES {
                row.patch = patch::empty_patch(&row.file);
            } else {
                total += bytes;
            }
            row
        })
        .collect()
}

/// `vcs.ts` git branch-metadata filter: a file named `HEAD` inside the realpath of the store.
pub fn is_branch_metadata(store: &Path, file: &Path) -> bool {
    crate::git::is_branch_metadata(store, file)
}

/// Approximates JS `String.prototype.localeCompare` (ICU root collation) for file paths:
/// punctuation < digits < letters, letters compared case-insensitively first, then lowercase
/// before uppercase, then code points.
pub fn locale_compare(a: &str, b: &str) -> Ordering {
    fn class(c: char) -> u8 {
        if c.is_alphabetic() {
            2
        } else if c.is_numeric() {
            1
        } else {
            0
        }
    }
    let primary = |s: &str| -> Vec<(u8, String)> {
        s.chars().map(|c| (class(c), c.to_lowercase().collect::<String>())).collect()
    };
    primary(a)
        .cmp(&primary(b))
        .then_with(|| {
            let tertiary = |s: &str| -> Vec<bool> { s.chars().map(|c| c.is_uppercase()).collect() };
            tertiary(a).cmp(&tertiary(b))
        })
        .then_with(|| a.cmp(b))
}

// ---------------------------------------------------------------------------- git runner

const CFG: &[&str] = &[
    "--no-optional-locks",
    "-c",
    "core.autocrlf=false",
    "-c",
    "core.fsmonitor=false",
    "-c",
    "core.longpaths=true",
    "-c",
    "core.symlinks=true",
    "-c",
    "core.quotepath=false",
];

static NAMED_REF_REJECT: Lazy<Regex> = Lazy::new(|| Regex::new(r"[~^:@{}\s]").unwrap());
static HEX_COMMIT: Lazy<Regex> = Lazy::new(|| Regex::new(r"^[a-f0-9]{40,64}$").unwrap());
static FULL_REF: Lazy<Regex> = Lazy::new(|| Regex::new(r"^refs/(heads|remotes)/.+").unwrap());
static REMOTE_PREFIX: Lazy<Regex> = Lazy::new(|| Regex::new(r"^refs/remotes/[^/]+/").unwrap());
static REFLOG: Lazy<Regex> = Lazy::new(|| Regex::new(r"^([a-f0-9]+)\x00([^\n\r]+)$").unwrap());

struct Out {
    exit_code: i32,
    text: String,
    truncated: bool,
}

#[derive(Clone, Default)]
struct Runner {
    index: Option<String>,
}

impl Runner {
    /// Spawn failures become exit code 1 with no output (`Effect.orElseSucceed`).
    async fn run(&self, args: Vec<String>, cwd: &Path, max: Option<usize>) -> Out {
        let mut full: Vec<String> = CFG.iter().map(|s| s.to_string()).collect();
        full.extend(args);
        let env = self.index.iter().map(|index| ("GIT_INDEX_FILE", index.clone())).collect();
        match process::run(cwd, &full, RunOptions { env, stdin: None, max_output_bytes: max }).await {
            Ok(result) => Out { exit_code: result.exit_code, text: result.text(), truncated: result.stdout_truncated },
            Err(_) => Out { exit_code: 1, text: String::new(), truncated: false },
        }
    }

    async fn text(&self, args: Vec<String>, cwd: &Path) -> String {
        self.run(args, cwd, None).await.text
    }

    async fn lines(&self, args: Vec<String>, cwd: &Path) -> Vec<String> {
        self.text(args, cwd).await.split('\n').map(|l| l.trim().to_string()).filter(|l| !l.is_empty()).collect()
    }
}

fn sv(items: &[&str]) -> Vec<String> {
    items.iter().map(|s| s.to_string()).collect()
}

fn kind(code: &str) -> Kind {
    if code == "??" {
        return Kind::Added;
    }
    if code.contains('U') {
        return Kind::Modified;
    }
    if code.contains('A') && !code.contains('D') {
        return Kind::Added;
    }
    if code.contains('D') && !code.contains('A') {
        return Kind::Deleted;
    }
    Kind::Modified
}

fn nuls(text: &str) -> Vec<&str> {
    text.split('\0').filter(|s| !s.is_empty()).collect()
}

/// `Number.parseInt(value || "0", 10)` with non-finite results as 0.
fn parse_int(value: &str) -> u64 {
    let value = if value.is_empty() { "0" } else { value };
    let digits: String = value.trim_start().chars().take_while(|c| c.is_ascii_digit()).collect();
    digits.parse().unwrap_or(0)
}

#[derive(Debug, Clone)]
struct Item {
    file: String,
    code: String,
    status: Kind,
}

#[derive(Debug, Clone, Copy)]
struct Stat {
    additions: u64,
    deletions: u64,
}

fn numstat(text: &str) -> Vec<(String, Stat)> {
    nuls(text)
        .into_iter()
        .filter_map(|item| {
            let a = item.find('\t')?;
            let b = a + 1 + item[a + 1..].find('\t')?;
            let file = &item[b + 1..];
            if file.is_empty() {
                return None;
            }
            let adds = &item[..a];
            let dels = &item[a + 1..b];
            Some((
                file.to_string(),
                Stat {
                    additions: if adds == "-" { 0 } else { parse_int(adds) },
                    deletions: if dels == "-" { 0 } else { parse_int(dels) },
                },
            ))
        })
        .collect()
}

fn nums(list: Vec<(String, Stat)>) -> HashMap<String, Stat> {
    // `new Map(entries)`: later duplicates win.
    list.into_iter().collect()
}

fn merge(lists: Vec<Vec<Item>>) -> Vec<Item> {
    let mut seen = HashSet::new();
    lists.into_iter().flatten().filter(|item| seen.insert(item.file.clone())).collect()
}

#[derive(Default)]
struct Batch {
    patches: HashMap<String, String>,
    capped: bool,
}

#[derive(Default)]
struct Untracked {
    stats: HashMap<String, Stat>,
    patches: HashMap<String, String>,
}

#[derive(Clone, Default)]
struct GitDiffOptions {
    context: Option<u64>,
    target: Option<&'static str>,
    untracked: Option<UntrackedMode>,
}

/// A folder listing that stopped early ends with an entry for the folder itself, marked `more`.
fn with_more(mut rows: Vec<FileDiff>, folder: Option<String>) -> Vec<FileDiff> {
    if let Some(folder) = folder {
        rows.push(FileDiff {
            file: folder,
            patch: String::new(),
            additions: 0,
            deletions: 0,
            status: Kind::Added,
            more: Some(true),
        });
    }
    rows
}

/// JS default `Array.prototype.sort`: UTF-16 code unit order.
fn utf16_cmp(a: &str, b: &str) -> Ordering {
    a.encode_utf16().cmp(b.encode_utf16())
}

/// One default branch: `name` (e.g. `main`) and `ref` (e.g. `origin/main`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DefaultBranch {
    pub name: String,
    pub reference: String,
}

/// The git VCS adapter for one Location.
#[derive(Debug, Clone)]
pub struct GitVcs {
    /// `location.directory`
    pub directory: PathBuf,
    /// `location.project.directory`
    pub worktree: PathBuf,
}

impl GitVcs {
    pub const ID: &'static str = "git";
    pub const NAME: &'static str = "Git";

    pub fn new(directory: impl Into<PathBuf>, worktree: impl Into<PathBuf>) -> Self {
        GitVcs { directory: directory.into(), worktree: worktree.into() }
    }

    fn runner(&self) -> Runner {
        Runner::default()
    }

    /// Provider `init`: plain `git init` in the worktree (no config prefix).
    pub async fn init(worktree: &Path) -> Result<(), DiffError> {
        let result = process::run(worktree, &sv(&["init"]), RunOptions::default()).await;
        match result {
            Ok(result) if result.exit_code == 0 => Ok(()),
            _ => Err(DiffError::new(INIT_FAILED_MESSAGE)),
        }
    }

    /// `info()`: current branch and default branch name. The `Vcs` service sets `provider`.
    pub async fn info(&self) -> Info {
        let (current, root) = tokio::join!(self.current_branch(), self.default_branch());
        Info { provider: None, branch: Branch { current, default: root.map(|r| r.name) } }
    }

    /// `git symbolic-ref --quiet --short HEAD`
    pub async fn current_branch(&self) -> Option<String> {
        let result =
            self.runner().run(sv(&["symbolic-ref", "--quiet", "--short", "HEAD"]), &self.directory, None).await;
        if result.exit_code != 0 {
            return None;
        }
        Some(result.text.trim().to_string()).filter(|v| !v.is_empty())
    }

    /// `for-each-ref` over heads and remotes, newest commit first; `*/HEAD` refs dropped.
    /// `refname:short` collapses `refs/remotes/<remote>/HEAD` to the bare remote name, so the
    /// namespace is stripped instead. Search and limit apply here because ref globs cannot cross
    /// `/` and `--count` precedes the filter.
    pub async fn branches(&self, options: &BranchOptions) -> Vec<String> {
        let search = options.search.as_deref().map(|s| s.trim().to_lowercase()).unwrap_or_default();
        let args = sv(&[
            "for-each-ref",
            "--sort=refname",
            "--sort=-committerdate",
            "--format=%(refname:lstrip=2)",
            "refs/heads",
            "refs/remotes",
        ]);
        let items = self
            .runner()
            .lines(args, &self.directory)
            .await
            .into_iter()
            .filter(|item| !item.ends_with("/HEAD") && (search.is_empty() || item.to_lowercase().contains(&search)));
        match options.limit {
            Some(limit) => items.take(limit as usize).collect(),
            None => items.collect(),
        }
    }

    async fn primary_remote(&self) -> Option<String> {
        let list = self.runner().lines(sv(&["remote"]), &self.directory).await;
        if list.iter().any(|r| r == "origin") {
            return Some("origin".to_string());
        }
        if list.len() == 1 {
            return list.into_iter().next();
        }
        if list.iter().any(|r| r == "upstream") {
            return Some("upstream".to_string());
        }
        list.into_iter().next()
    }

    /// Remote HEAD of the primary remote, else `init.defaultBranch`, `main` or `master` when present.
    pub async fn default_branch(&self) -> Option<DefaultBranch> {
        let runner = self.runner();
        let cwd = &self.directory;
        if let Some(remote) = self.primary_remote().await {
            let head = runner.run(vec!["symbolic-ref".into(), format!("refs/remotes/{remote}/HEAD")], cwd, None).await;
            if head.exit_code == 0 {
                let trimmed = head.text.trim();
                let reference = trimmed.strip_prefix("refs/remotes/").unwrap_or(trimmed).to_string();
                let name = reference.strip_prefix(&format!("{remote}/")).unwrap_or("").to_string();
                if !name.is_empty() {
                    return Some(DefaultBranch { name, reference });
                }
            }
        }
        let list = runner.lines(sv(&["for-each-ref", "--format=%(refname:lstrip=2)", "refs/heads"]), cwd).await;
        let configured = runner.run(sv(&["config", "init.defaultBranch"]), cwd, None).await;
        let name = configured.text.trim();
        if !name.is_empty() && list.iter().any(|b| b == name) {
            return Some(DefaultBranch { name: name.to_string(), reference: name.to_string() });
        }
        for candidate in ["main", "master"] {
            if list.iter().any(|b| b == candidate) {
                return Some(DefaultBranch { name: candidate.to_string(), reference: candidate.to_string() });
            }
        }
        None
    }

    async fn resolve(&self, reference: &str) -> Option<String> {
        let result = self
            .runner()
            .run(
                vec!["rev-parse".into(), "--verify".into(), "--end-of-options".into(), format!("{reference}^{{commit}}")],
                &self.directory,
                None,
            )
            .await;
        if result.exit_code != 0 {
            return None;
        }
        Some(result.text.trim().to_string()).filter(|v| !v.is_empty())
    }

    async fn ancestor(&self, commit: &str, reference: &str) -> bool {
        if !HEX_COMMIT.is_match(commit) {
            return false;
        }
        self.runner().run(sv(&["merge-base", "--is-ancestor", commit, reference]), &self.directory, None).await.exit_code
            == 0
    }

    /// Creation hints must identify a branch, not HEAD, an object ID, or a revision expression.
    async fn named_ref(&self, input: &str) -> Option<(String, String)> {
        if input == "HEAD" || input.ends_with("/HEAD") || NAMED_REF_REJECT.is_match(input) {
            return None;
        }
        let text = self
            .runner()
            .text(sv(&["rev-parse", "--symbolic-full-name", "--verify", "--end-of-options", input]), &self.directory)
            .await;
        let reference = text.trim().to_string();
        if !FULL_REF.is_match(&reference) || reference.ends_with("/HEAD") || self.resolve(&reference).await.is_none() {
            return None;
        }
        let name = reference.strip_prefix("refs/heads/").unwrap_or(&reference).to_string();
        let name = REMOTE_PREFIX.replace(&name, "").into_owned();
        Some((name, reference))
    }

    /// `git rev-parse --verify HEAD`
    pub async fn has_head(&self) -> bool {
        self.runner().run(sv(&["rev-parse", "--verify", "HEAD"]), &self.directory, None).await.exit_code == 0
    }

    async fn merge_base(&self, base: &str) -> Option<String> {
        let reference = self.resolve(base).await?;
        let result = self.runner().run(sv(&["merge-base", &reference, "HEAD"]), &self.directory, None).await;
        if result.exit_code != 0 {
            return None;
        }
        Some(result.text.trim().to_string()).filter(|v| !v.is_empty())
    }

    /// `base()`: the review base from the branch's creation reflog entry, or the default branch
    /// when it is checked out. `Ok(None)` when there is no HEAD.
    pub async fn base(&self) -> Result<Option<Base>, DiffError> {
        if !self.has_head().await {
            return Ok(None);
        }
        let Some(current) = self.current_branch().await else {
            return Err(DiffError::new("Choose a review base"));
        };
        let history: Vec<(String, String)> = self
            .runner()
            .lines(
                vec![
                    "reflog".into(),
                    "show".into(),
                    "--max-count=256".into(),
                    "--format=%H%x00%gs".into(),
                    format!("refs/heads/{current}"),
                ],
                &self.directory,
            )
            .await
            .into_iter()
            .filter_map(|line| REFLOG.captures(&line).map(|c| (c[1].to_string(), c[2].to_string())))
            .collect();
        let renamed = history.iter().any(|(_, message)| message.starts_with("Branch: renamed "));
        let creation = if renamed {
            None
        } else {
            history.iter().find(|(_, message)| message.starts_with("branch: Created from "))
        };
        if let Some((commit, message)) = creation {
            let origin = &message["branch: Created from ".len()..];
            if !origin.is_empty() {
                if let Some((name, reference)) = self.named_ref(origin).await {
                    if name != current && self.ancestor(commit, "HEAD").await && self.ancestor(commit, &reference).await {
                        return Ok(Some(Base { name, reference, source: BaseSource::Reflog }));
                    }
                }
            }
        }
        let root = self.default_branch().await;
        let Some(root) = root.filter(|root| root.name == current) else {
            return Err(DiffError::new("Choose a review base"));
        };
        let Some((_, reference)) = self.named_ref(&root.reference).await else {
            return Err(DiffError::new("The default review base is unavailable"));
        };
        Ok(Some(Base { name: root.name, reference, source: BaseSource::Default }))
    }

    /// `statusNames`: `git status` with `--untracked-files=<normal|all>`, over `.` or the given
    /// directory entries.
    async fn status_items(&self, untracked: &str, names: Option<&[String]>) -> Vec<Item> {
        let mut args = sv(&["status", "--porcelain=v1"]);
        args.push(format!("--untracked-files={untracked}"));
        args.extend(sv(&["--no-renames", "-z", "--"]));
        match names {
            Some(names) => args.extend(names.iter().map(|name| format!(":(literal){name}"))),
            None => args.push(".".to_string()),
        }
        let text = self.runner().text(args, &self.directory).await;
        nuls(&text)
            .into_iter()
            .filter_map(|item| {
                let file = item.get(3..).unwrap_or("");
                if file.is_empty() {
                    return None;
                }
                let code = item.get(..2).unwrap_or(item).to_string();
                Some(Item { file: file.to_string(), status: kind(&code), code })
            })
            .collect()
    }

    /// `statusItems`: untracked files one by one unless the caller asks for folders. With folders,
    /// Git lists an untracked folder as one `dir/` entry, as `git status` shows it, and a directory
    /// inside such a folder lists only its immediate entries, with its subfolders again as one
    /// entry each. The second value is the folder's own `more` row when the listing stopped early.
    async fn status_list(&self, untracked: UntrackedMode) -> (Vec<Item>, Option<String>) {
        if untracked == UntrackedMode::Files {
            return (self.status_items("all", None).await, None);
        }
        let list = self.status_items("normal", None).await;
        let prefix = crate::paths::relative(&self.worktree, &self.directory).replace(std::path::MAIN_SEPARATOR, "/");
        let folder = format!("{prefix}/");
        if prefix.is_empty()
            || !list.iter().any(|item| item.code == "??" && item.file.ends_with('/') && folder.starts_with(&item.file))
        {
            return (list, None);
        }
        let mut names: Vec<String> = match std::fs::read_dir(&self.directory) {
            Ok(entries) => entries.flatten().map(|entry| entry.file_name().to_string_lossy().into_owned()).collect(),
            Err(_) => Vec::new(),
        };
        names.sort_by(|a, b| utf16_cmp(a, b));
        // Ignored entries do not list, so check entries a batch at a time until the listing fills,
        // the folder runs out, or the scan budget does.
        let mut entries: Vec<Item> = Vec::new();
        let started = std::time::Instant::now();
        let mut checked = 0usize;
        while checked < names.len()
            && entries.len() < MAX_FOLDER_ENTRIES
            && checked < MAX_FOLDER_SCAN
            && started.elapsed().as_millis() < MAX_FOLDER_SCAN_MS
        {
            let end = (checked + MAX_FOLDER_ENTRIES).min(names.len());
            let batch = &names[checked..end];
            checked += batch.len();
            entries.extend(self.status_items("normal", Some(batch)).await);
        }
        let more = (entries.len() > MAX_FOLDER_ENTRIES || checked < names.len()).then_some(folder);
        entries.truncate(MAX_FOLDER_ENTRIES);
        (entries, more)
    }

    async fn diff_names(&self, reference: &str, target: Option<&str>) -> Result<Vec<Item>, DiffError> {
        let mut args = sv(&["diff", "--no-ext-diff", "--no-renames", "--name-status", "-z", reference]);
        args.extend(target.map(str::to_string));
        args.extend(sv(&["--", "."]));
        let result = self.runner().run(args, &self.directory, None).await;
        if result.exit_code != 0 {
            return Err(DiffError::new("Unable to list Git changes"));
        }
        let list = nuls(&result.text);
        Ok(list
            .iter()
            .enumerate()
            .filter_map(|(index, code)| {
                if index % 2 != 0 {
                    return None;
                }
                let file = list.get(index + 1)?;
                Some(Item { file: file.to_string(), code: code.to_string(), status: kind(code) })
            })
            .collect())
    }

    async fn stats(&self, reference: &str, target: Option<&str>) -> Vec<(String, Stat)> {
        let mut args = sv(&["diff", "--no-ext-diff", "--no-renames", "--numstat", "-z", reference]);
        args.extend(target.map(str::to_string));
        args.extend(sv(&["--", "."]));
        numstat(&self.runner().text(args, &self.directory).await)
    }

    fn unified(context: Option<u64>) -> String {
        format!("--unified={}", context.unwrap_or(3))
    }

    async fn patch_file(&self, reference: &str, file: &str, options: &GitDiffOptions, max: usize) -> Patch {
        let mut args = sv(&["diff", "--patch", "--no-ext-diff", "--no-renames"]);
        args.push(Self::unified(options.context));
        args.push(reference.to_string());
        args.extend(options.target.map(str::to_string));
        args.extend(["--".to_string(), file.to_string()]);
        let result = self.runner().run(args, &self.worktree, Some(max)).await;
        Patch { text: if result.truncated { String::new() } else { result.text }, truncated: result.truncated }
    }

    async fn patch_all(&self, reference: &str, options: &GitDiffOptions, max: usize) -> Patch {
        let mut args = sv(&["diff", "--patch", "--no-ext-diff", "--no-renames"]);
        args.push(Self::unified(options.context));
        args.push(reference.to_string());
        args.extend(options.target.map(str::to_string));
        args.extend(sv(&["--", "."]));
        let result = self.runner().run(args, &self.directory, Some(max)).await;
        Patch { text: result.text, truncated: result.truncated }
    }

    async fn patch_untracked(&self, file: &str, context: Option<u64>, max: usize) -> Patch {
        // A folder entry has no patch of its own.
        if file.ends_with('/') {
            return Patch { text: String::new(), truncated: false };
        }
        let mut args = sv(&["diff", "--no-index", "--patch", "--no-ext-diff", "--no-renames"]);
        args.push(Self::unified(context));
        args.extend(["--".to_string(), "/dev/null".to_string(), file.to_string()]);
        let result = self.runner().run(args, &self.worktree, Some(max)).await;
        Patch { text: if result.truncated { String::new() } else { result.text }, truncated: result.truncated }
    }

    async fn stat_untracked(&self, file: &str) -> Option<Stat> {
        if file.ends_with('/') {
            return None;
        }
        let result = self
            .runner()
            .run(sv(&["diff", "--no-index", "--numstat", "--", "/dev/null", file]), &self.worktree, Some(4096))
            .await;
        if result.truncated {
            return None;
        }
        let parts: Vec<&str> = result.text.split('\t').collect();
        if parts.len() < 2 {
            return None;
        }
        Some(Stat {
            additions: if parts[0] == "-" { 0 } else { parse_int(parts[0]) },
            deletions: if parts[1] == "-" { 0 } else { parse_int(parts[1]) },
        })
    }

    /// Prefetches untracked stats (and patches when `patches` is set) in up to three Git processes,
    /// by recording the files intent-to-add in a throwaway index.
    async fn untracked_batch(&self, files: &[String], patches: Option<&GitDiffOptions>) -> Untracked {
        let mut eligible = Vec::new();
        for file in files {
            // Directories (nested repositories) and symlinks diff differently through an index.
            if file.ends_with('/') {
                continue;
            }
            let Ok(stat) = tokio::fs::symlink_metadata(self.worktree.join(file)).await else { continue };
            // A file this large could crowd every other file out of the batch's output budget.
            if !stat.is_file() || stat.len() > (MAX_TOTAL_PATCH_BYTES / 2) as u64 {
                continue;
            }
            eligible.push(file.clone());
        }
        // The batch costs up to three processes; for a single file the per-file commands are cheaper.
        if eligible.len() < 2 {
            return Untracked::default();
        }
        let base = std::env::temp_dir().join(format!("opencode-untracked-{}", uuid_v4()));
        let list = PathBuf::from(format!("{}.list", base.display()));
        let index = format!("{}.index", base.display());
        let _cleanup = Cleanup(vec![list.clone(), PathBuf::from(&index), PathBuf::from(format!("{index}.lock"))]);
        if tokio::fs::write(&list, eligible.iter().map(|f| format!("{f}\0")).collect::<String>()).await.is_err() {
            return Untracked::default();
        }
        let runner = Runner { index: Some(index.clone()) };
        // Paths are literal so names containing pathspec magic such as `*` or `[` match only themselves.
        runner
            .run(
                vec![
                    "--literal-pathspecs".into(),
                    "-c".into(),
                    "core.splitIndex=false".into(),
                    "add".into(),
                    "--intent-to-add".into(),
                    "--force".into(),
                    "--sparse".into(),
                    "--ignore-errors".into(),
                    format!("--pathspec-from-file={}", list.display()),
                    "--pathspec-file-nul".into(),
                ],
                &self.worktree,
                None,
            )
            .await;
        let stats_future = runner.text(sv(&["diff", "--no-ext-diff", "--no-renames", "--numstat", "-z"]), &self.worktree);
        let patch_future = async {
            match patches {
                Some(options) => {
                    let mut args = sv(&["diff", "--patch", "--no-ext-diff", "--no-renames"]);
                    args.push(Self::unified(Some(options.context.unwrap_or(PATCH_CONTEXT_LINES))));
                    let result = runner.run(args, &self.worktree, Some(MAX_TOTAL_PATCH_BYTES)).await;
                    Patch { text: result.text, truncated: result.truncated }
                }
                None => Patch::default(),
            }
        };
        let (stats_text, patch) = tokio::join!(stats_future, patch_future);
        let wanted: HashSet<&String> = eligible.iter().collect();
        let headers: HashMap<String, &String> =
            eligible.iter().map(|file| (format!("diff --git a/{file} b/{file}"), file)).collect();
        let stats = numstat(&stats_text).into_iter().filter(|(file, _)| wanted.contains(file)).collect();
        // Only chunks with the default `a/` and `b/` prefixes match the per-file output.
        let patches = patch::split_git_patch(&patch)
            .into_iter()
            .filter_map(|chunk| {
                let header = match chunk.find('\n') {
                    Some(end) => &chunk[..end],
                    None => {
                        let mut chars = chunk.chars();
                        chars.next_back();
                        chars.as_str()
                    }
                };
                headers.get(header).map(|file| ((*file).clone(), chunk.to_string()))
            })
            .collect();
        Untracked { stats, patches }
    }

    /// `status()`: working-tree status with line counts, sorted like `localeCompare`.
    pub async fn status(&self) -> Vec<VcsFileStatus> {
        let reference = if self.has_head().await { Some("HEAD") } else { None };
        let (list, stats) = tokio::join!(self.status_items("all", None), async {
            match reference {
                Some(reference) => self.stats(reference, None).await,
                None => Vec::new(),
            }
        });
        let map = nums(stats);
        let candidates: Vec<String> = list
            .iter()
            .filter(|item| item.status == Kind::Added && !map.contains_key(&item.file))
            .map(|item| item.file.clone())
            .collect();
        let untracked = self.untracked_batch(&candidates, None).await;
        let mut sorted = list;
        sorted.sort_by(|a, b| locale_compare(&a.file, &b.file));
        let mut out = Vec::new();
        for item in sorted {
            let stat = match map.get(&item.file) {
                Some(stat) => Some(*stat),
                None if item.status == Kind::Added => match untracked.stats.get(&item.file) {
                    Some(stat) => Some(*stat),
                    None => self.stat_untracked(&item.file).await,
                },
                None => None,
            };
            out.push(VcsFileStatus {
                file: item.file,
                additions: stat.map(|s| s.additions).unwrap_or(0),
                deletions: stat.map(|s| s.deletions).unwrap_or(0),
                status: item.status,
            });
        }
        out
    }

    /// `diff(mode, {context, base})` as the provider implements it. The `Vcs` service then applies
    /// [`apply_total_patch_budget`] (see [`GitVcs::review_diff`]).
    pub async fn diff(&self, mode: Mode, options: &DiffOptions) -> Result<Vec<FileDiff>, DiffError> {
        let git_options = GitDiffOptions { context: options.context, target: None, untracked: options.untracked };
        if mode == Mode::Working {
            let reference = if self.has_head().await { Some("HEAD") } else { None };
            return self.track(reference, &git_options).await;
        }
        if !self.has_head().await {
            return if mode == Mode::Committed { Ok(Vec::new()) } else { self.track(None, &git_options).await };
        }
        let base = match &options.base {
            Some(base) => Some(base.clone()),
            None => self.default_branch().await.map(|root| root.reference),
        };
        let reference = match &base {
            Some(base) => self.merge_base(base).await,
            None => None,
        };
        let Some(reference) = reference else {
            return Err(DiffError::new(match base {
                Some(base) => format!("No merge base available for {base}"),
                None => "No review base available".to_string(),
            }));
        };
        let options = GitDiffOptions {
            context: options.context,
            target: if mode == Mode::Committed { Some("HEAD") } else { None },
            untracked: options.untracked,
        };
        self.diff_against_ref(&reference, &options).await
    }

    /// `Vcs.diff`: provider diff with `context ?? PATCH_CONTEXT_LINES`, then the 10 MB total budget.
    pub async fn review_diff(&self, mode: Mode, options: &DiffOptions) -> Result<Vec<FileDiff>, DiffError> {
        let options = DiffOptions {
            context: Some(options.context.unwrap_or(PATCH_CONTEXT_LINES)),
            base: options.base.clone(),
            untracked: options.untracked,
        };
        Ok(apply_total_patch_budget(self.diff(mode, &options).await?))
    }

    async fn track(&self, reference: Option<&str>, options: &GitDiffOptions) -> Result<Vec<FileDiff>, DiffError> {
        match reference {
            None => {
                let (list, more) = self.status_list(options.untracked.unwrap_or_default()).await;
                Ok(with_more(self.files(None, list, HashMap::new(), Batch::default(), options).await, more))
            }
            Some(reference) => self.diff_against_ref(reference, options).await,
        }
    }

    async fn diff_against_ref(&self, reference: &str, options: &GitDiffOptions) -> Result<Vec<FileDiff>, DiffError> {
        let (list, stats, (extra, more)) = tokio::join!(
            self.diff_names(reference, options.target),
            self.stats(reference, options.target),
            async {
                if options.target.is_some() {
                    (Vec::new(), None)
                } else {
                    self.status_list(options.untracked.unwrap_or_default()).await
                }
            }
        );
        let list = list?;
        let batch = self.batch_patches(reference, &list, options).await;
        let merged = merge(vec![list, extra.into_iter().filter(|item| item.code == "??").collect()]);
        Ok(with_more(self.files(Some(reference), merged, nums(stats), batch, options).await, more))
    }

    async fn batch_patches(&self, reference: &str, list: &[Item], options: &GitDiffOptions) -> Batch {
        if list.is_empty() {
            return Batch::default();
        }
        let result = self
            .patch_all(
                reference,
                &GitDiffOptions { context: Some(options.context.unwrap_or(PATCH_CONTEXT_LINES)), target: options.target, untracked: None },
                MAX_TOTAL_PATCH_BYTES,
            )
            .await;
        Batch {
            patches: patch::chunks_by_file(&result, |index| list.get(index).map(|item| item.file.clone())),
            capped: result.truncated,
        }
    }

    async fn native_patch(&self, reference: Option<&str>, item: &Item, options: &GitDiffOptions) -> String {
        let context = Some(options.context.unwrap_or(PATCH_CONTEXT_LINES));
        let result = match reference {
            Some(reference) if item.code != "??" => {
                self.patch_file(reference, &item.file, &GitDiffOptions { context, target: options.target, untracked: None }, MAX_PATCH_BYTES)
                    .await
            }
            _ => self.patch_untracked(&item.file, context, MAX_PATCH_BYTES).await,
        };
        if !result.truncated && !result.text.is_empty() {
            return result.text;
        }
        patch::empty_patch(&item.file)
    }

    async fn patch_for_item(
        &self,
        reference: Option<&str>,
        item: &Item,
        batch: &Batch,
        untracked: &HashMap<String, String>,
        capped: bool,
        options: &GitDiffOptions,
    ) -> String {
        if capped {
            return patch::empty_patch(&item.file);
        }
        if let Some(batched) = batch.patches.get(&item.file) {
            return batched.clone();
        }
        if item.code != "??" && batch.capped {
            return patch::empty_patch(&item.file);
        }
        if item.code == "??" || reference.is_none() {
            if let Some(prefetched) = untracked.get(&item.file) {
                return prefetched.clone();
            }
        }
        self.native_patch(reference, item, options).await
    }

    async fn files(
        &self,
        reference: Option<&str>,
        list: Vec<Item>,
        map: HashMap<String, Stat>,
        batch: Batch,
        options: &GitDiffOptions,
    ) -> Vec<FileDiff> {
        let candidates: Vec<String> = list
            .iter()
            .filter(|item| {
                item.code == "??"
                    || reference.is_none()
                    || (options.target.is_none() && item.status == Kind::Added && !map.contains_key(&item.file))
            })
            .map(|item| item.file.clone())
            .collect();
        let untracked = self.untracked_batch(&candidates, Some(options)).await;
        let mut sorted = list;
        sorted.sort_by(|a, b| locale_compare(&a.file, &b.file));
        let mut next = Vec::new();
        let mut total = 0usize;
        let mut capped = false;
        for item in sorted {
            let stat = match map.get(&item.file) {
                Some(stat) => Some(*stat),
                None if options.target.is_none() && item.status == Kind::Added => {
                    match untracked.stats.get(&item.file) {
                        Some(stat) => Some(*stat),
                        None => self.stat_untracked(&item.file).await,
                    }
                }
                None => None,
            };
            let patch_text =
                self.patch_for_item(reference, &item, &batch, &untracked.patches, capped, options).await;
            let (patch_text, result_capped) = if capped {
                (patch_text, true)
            } else if total + patch_text.len() <= MAX_TOTAL_PATCH_BYTES {
                (patch_text, false)
            } else {
                (patch::empty_patch(&item.file), true)
            };
            capped = capped || result_capped;
            if !capped {
                total += patch_text.len();
                capped = total >= MAX_TOTAL_PATCH_BYTES;
            }
            next.push(FileDiff {
                file: item.file,
                patch: patch_text,
                additions: stat.map(|s| s.additions).unwrap_or(0),
                deletions: stat.map(|s| s.deletions).unwrap_or(0),
                status: item.status,
                more: None,
            });
        }
        next
    }
}

struct Cleanup(Vec<PathBuf>);

impl Drop for Cleanup {
    fn drop(&mut self) {
        for file in &self.0 {
            let _ = std::fs::remove_file(file);
        }
    }
}

fn uuid_v4() -> String {
    let bytes: [u8; 16] = rand::random();
    let hex: String = bytes.iter().map(|b| format!("{b:02x}")).collect();
    format!("{}-4{}-a{}-{}-{}", &hex[0..8], &hex[13..16], &hex[17..20], &hex[20..24], &hex[24..32])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn status_codes() {
        assert_eq!(kind("??"), Kind::Added);
        assert_eq!(kind("UU"), Kind::Modified);
        assert_eq!(kind("A "), Kind::Added);
        assert_eq!(kind(" D"), Kind::Deleted);
        assert_eq!(kind("AD"), Kind::Modified);
        assert_eq!(kind(" M"), Kind::Modified);
    }

    #[test]
    fn parses_numstat() {
        let parsed = numstat("1\t2\ta.txt\0-\t-\tbin\0bad\0");
        assert_eq!(parsed.len(), 2);
        assert_eq!((parsed[0].1.additions, parsed[0].1.deletions), (1, 2));
        assert_eq!((parsed[1].1.additions, parsed[1].1.deletions), (0, 0));
    }

    #[test]
    fn locale_order() {
        let mut files = vec!["b.txt", "B.txt", "a.txt", "_x", "1.txt", "A.txt"];
        files.sort_by(|a, b| locale_compare(a, b));
        assert_eq!(files, vec!["_x", "1.txt", "a.txt", "A.txt", "b.txt", "B.txt"]);
    }

    #[test]
    fn total_budget_empties_later_patches() {
        let big = "x".repeat(MAX_TOTAL_PATCH_BYTES - 10);
        let row = |file: &str, patch: &str| FileDiff {
            file: file.into(),
            patch: patch.into(),
            additions: 0,
            deletions: 0,
            status: Kind::Modified,
            more: None,
        };
        let rows = apply_total_patch_budget(vec![row("a", &big), row("b", &"y".repeat(20)), row("c", "z")]);
        assert_eq!(rows[0].patch.len(), big.len());
        assert_eq!(rows[1].patch, patch::empty_patch("b"));
        assert_eq!(rows[2].patch, "z");
    }

    #[test]
    fn serializes_schema_shapes() {
        let base = Base { name: "main".into(), reference: "origin/main".into(), source: BaseSource::Default };
        assert_eq!(serde_json::to_string(&base).unwrap(), r#"{"name":"main","ref":"origin/main","source":"default"}"#);
        let info = Info { provider: Some("git".into()), branch: Branch { current: Some("dev".into()), default: None } };
        assert_eq!(serde_json::to_string(&info).unwrap(), r#"{"provider":"git","branch":{"current":"dev"}}"#);
        let status = VcsFileStatus { file: "a".into(), additions: 1, deletions: 0, status: Kind::Added };
        assert_eq!(
            serde_json::to_string(&status).unwrap(),
            r#"{"file":"a","additions":1,"deletions":0,"status":"added"}"#
        );
    }
}
