# oc-git

Git operations for the opencode Rust port. A port of upstream `packages/core/src/git.ts`,
`snapshot.ts`, `repository.ts`, `repository-cache.ts`, `file-retention.ts`, the git strategy of
`worktree.ts` (`worktree/{git,directory}.ts`, `util/slug.ts`), `vcs/patch.ts`, and the git VCS
provider `plugin/vcs/git.ts` (with the provider-independent parts of `vcs.ts`).

Every operation runs Git (`process::git_executable()`: `git`, except that on macOS the
`/usr/bin/git` shim is swapped for the developer tools' own Git, and Windows uses the absolute
`which git`) through `tokio::process`, with the same arguments, working
directory and environment as upstream. Store commands run as `git --git-dir <store> --work-tree
<worktree> …` with `cwd = worktree`. Index writes use `GIT_INDEX_FILE`. Pathspecs use
`:(literal)`, output is parsed with `-z`, and diffs use `--literal-pathspecs` and
`-c core.quotepath=false`.

## Modules

| module | upstream | contents |
|---|---|---|
| `git` | `git.ts` | `Git` service, `Repository`, `TreeId`, `OperationError`, `WorktreeError`, `Worktree` |
| `snapshot` | `snapshot.ts` | `Snapshots` (one per Location), `SnapshotLocation`, `SnapshotError` |
| `repository` | `repository.ts` | `parse`, `parse_remote`, `validate_branch`, `cache_path`, `cache_identity`, `same`, `encode_uri_component` |
| `repository_cache` | `repository-cache.ts` | `RepositoryCache::ensure`, `Kv` trait (+ `MemoryKv`), `CacheError` |
| `worktree` | `worktree/git.ts`, `worktree/directory.ts`, `util/slug.ts` | `GitWorktreeStrategy` (create/remove/list), `slug`, `default_directory`, `allocate_directory`, `canonical` |
| `file_retention` | `file-retention.ts` | `cleanup(files, retention)` |
| `vcs` | `plugin/vcs/git.ts`, `vcs.ts`, `schema/src/vcs.ts` | `GitVcs` adapter (info/base/branches/status/diff/init), `Info`, `Branch`, `Base`, `Mode`, `VcsFileStatus`, `DiffError`, `apply_total_patch_budget`, `locale_compare` |
| `patch` | `vcs/patch.ts` | `empty_patch` (byte-identical to `diff@8.0.4`), `split_git_patch`, `chunks_by_file`, `count_patch`, limits |
| `paths` | Node `path` / `FSUtil` | `normalize`, `resolve`, `relative`, `contains`, `real_or_resolved` |
| `process` | `AppProcess.run` | the `git` runner (stdin bytes, output cap with a truncation flag) |

## `Git` (cheap `Clone`; clones share the in-process per-store locks and the fast-path cache)

```rust
// repo
async fn discover(&self, dir: &Path) -> Option<Repository>;           // nearest .git, rev-parse
async fn clone_repository(&self, CloneInput) -> Result<Repository, OperationError>; // --depth 100
async fn create(&self, worktree, git_directory, seed: Option<&Repository>) -> Result<Repository, OperationError>;
// remote / history
async fn fingerprint(&self, dot_git: &Path) -> Option<String>;       // spawn-free metadata stamp (`git::fingerprint`)
async fn remote(&self, &Repository, name: Option<&str>) -> Result<Option<String>, CommandError>; // exit 2 = no remote
async fn head / branch(&self, &Repository) -> Option<String>;
async fn default_remote_branch(&self, &Repository, remote: Option<&str>) -> Option<String>;
async fn root_commits(&self, &Repository) -> Result<Vec<String>, CommandError>; // sorted; [] when HEAD is unborn
// sync (repository cache)
async fn fetch_remotes(&self, &Repository, prune: Option<bool>) -> Result<(), OperationError>;
async fn fetch_branch(&self, &Repository, FetchBranchInput) -> Result<(), OperationError>;
async fn checkout_remote_branch(&self, &Repository, CheckoutInput) -> Result<(), OperationError>;
async fn reset_hard(&self, &Repository, revision: &str) -> Result<(), OperationError>;
// worktrees
async fn worktree_create(&self, &Repository, dir, reference: Option<&str>) -> Result<Repository, WorktreeError>;
async fn worktree_remove(&self, &Repository, dir, force: bool) -> Result<(), WorktreeError>; // force_required
async fn worktree_list(&self, &Repository) -> Result<Vec<Worktree>, WorktreeError>;          // main first
// index / trees / objects (snapshot store)
async fn refresh(&self, &Repository, scope, ignores: Option<&Repository>, max_untracked: Option<u64>) -> Result<RefreshResult, OperationError>;
async fn ignored(&self, &Repository, paths: &[String]) -> Result<HashSet<String>, OperationError>;
async fn capture(&self, &CaptureInput) -> Result<TreeId, OperationError>;
async fn write_tree(&self, &Repository) -> Result<TreeId, OperationError>;
async fn tree_files(&self, &Repository, from, to) -> Result<Vec<String>, OperationError>;
async fn tree_diff(&self, &Repository, &DiffInput) -> Result<Vec<oc_schema::types::FileDiff>, OperationError>;
async fn restore(&self, &Repository, files: &[(String, TreeId)]) -> Result<(), OperationError>; // ordered like a JS Map
async fn pack(&self, &Repository) -> Result<(), OperationError>;
```

Free helpers: `nuls`, `literal_pathspec`, `can_batch_restore`, `path_groups`,
`pack_index_object_ids`, `unique_suffix`, `is_branch_metadata`, plus `SNAPSHOT_CONFIG` and
`SNAPSHOT_CONFIG_INCLUDE`.

## `Snapshots`

```rust
let snapshots = Snapshots::new(git, SnapshotLocation { data, project_id, project_directory, directory, vcs });
snapshots.capture().await                          // Option<TreeId>; None if disabled, not git, or the capture failed
snapshots.files(&from, &to).await                  // Result<Vec<String>, SnapshotError>
snapshots.diff(&from, &to, context, paths).await   // Result<Vec<FileDiff>, SnapshotError>
snapshots.restore(&[(path, tree), ...]).await      // absent paths are deleted
snapshots.pack().await                             // capture also runs this in the background, at most every 10 min
snapshots.configure(enabled)                       // State editor `configure`
```

`data` is `Global.data` (`<XDG_DATA_HOME>/opencode`). The store lives at
`<data>/snapshot/<projectID>/<sha1(realpath(worktree))>`. The store repository is discovered and
created lazily, once, in its own task. Its result is cached, including failure, so a cancelled
caller cannot poison it.

## `GitVcs` (the `opencode.vcs.git` provider)

```rust
let vcs = GitVcs::new(location_directory, project_directory);
GitVcs::init(&worktree).await                    // `git init`; error "Git initialization failed"
vcs.info().await                                 // Info { provider: None, branch: { current, default } }
vcs.base().await                                 // Result<Option<Base>, DiffError>; None without HEAD
vcs.branches(&BranchOptions { search, limit }).await   // Vec<String>: `refname:lstrip=2`, searched and limited here
vcs.status().await                               // Vec<VcsFileStatus>, sorted like localeCompare
vcs.diff(mode, &DiffOptions { context, base, untracked }).await // provider result; context None = full file (2147483647 lines)
vcs.review_diff(mode, &DiffOptions { .. }).await // what `Vcs.diff` returns: context ?? 2147483647, then the 10 MB total budget
```

How oc-core's `Vcs` service and the HTTP `vcs.*` endpoints use it:

- `vcs.get` returns `info()` with `provider: Some("git")` set by the caller.
- `vcs.base` maps to `base()`. Other provider errors become `BASE_FAILED_MESSAGE`, and
  `vcs.diff` errors become `DIFF_FAILED_MESSAGE`.
- `vcs.status` maps to `status()`.
- `vcs.branch.list` maps to `branches({search, limit: min(limit ?? 50, 100)})`.
- `vcs.diff` maps to `review_diff(mode, {context, base, untracked})`. With `untracked: Folders` an
  untracked folder is one `dir/` row (no patch, no counts); inside one, its entries list one level
  deep: at most 500 rows from at most 5000 checked entries in 1 s, then a
  `{file: "dir/", patch: "", more: true}` row when it stopped early.
- `vcs.init` maps to `GitVcs::init`.
- `vcs::is_branch_metadata(store, file)` decides whether a `filesystem.changed` event should
  refresh branch info.

Behavior that matches upstream:

- Every command is prefixed with `--no-optional-locks -c core.autocrlf=false -c
  core.fsmonitor=false -c core.longpaths=true -c core.symlinks=true -c core.quotepath=false`.
- Listings run in the Location directory with the pathspec `.`, and per-file commands run in the
  worktree root.
- The default branch comes from the primary remote's `HEAD`. Otherwise it is `init.defaultBranch`,
  then `main`, then `master`, but only if that branch exists.
- The review base comes from the reflog `branch: Created from <x>` entry. That entry is skipped
  after a `Branch: renamed` entry, and `x` must be a named ref, not HEAD or an expression. The
  fallback is the default branch, and only while that branch is checked out.
- Patches are batched into a single `git diff`. Each file has a 10 MB cap and the total is 10 MB;
  beyond that, files get `empty_patch`.
- Untracked files are batched through a throwaway `GIT_INDEX_FILE` (`$TMPDIR/opencode-untracked-<uuid>.{list,index}`)
  using `add --intent-to-add`. Per-file `diff --no-index` is the fallback.
- `locale_compare` approximates ICU root collation: punctuation, then digits, then letters;
  case-insensitive first, with lowercase before uppercase.

## Behavior kept from upstream

- **Store creation**
  - `git init` runs in `<store>.init-<pid>-<rand>`, which is then renamed into place.
  - `opencode.gitconfig` is written byte-identical to upstream.
  - `[include]\n\tpath = opencode.gitconfig\n` is appended to `config`, separated by a blank
    line, unless it is already present.
  - `objects/info/alternates` is set to `<seed common dir>/objects\n`.
  - The seed's index is copied, best-effort, keeping its mtime. For a split index the source's
    `sharedindex.*` files are linked in just long enough for `update-index --no-split-index`
    (`seedIndex`, also used to rebuild an unreadable index).
  - On first use in a process, an outdated include file is rewritten atomically.
- **Capture**
  - A temporary index `index.opencode-<ms>-<pid>-<rand>` is created by hard-linking the store
    index, falling back to a copy that keeps the mtime.
  - Changes are listed with `diff-files` and `ls-files --others [--exclude-from=<source
    info/exclude>] --exclude-standard`, both run concurrently.
  - Captures queued behind the store lock share the next run (keyed by repository, scopes,
    ignores and size limit); a caller arriving while one runs queues a new one.
  - Paths are filtered through the source repository's `check-ignore --no-index --stdin -z`
    (exit codes 0 and 1 both succeed).
  - Untracked files over 2 MiB are skipped and remembered by `size:mtime`, so later captures
    neither re-check them against the ignore rules nor rewrite the tree for them.
  - Index updates use `update-index --force-remove` and `update-index --add --remove --replace`,
    and embedded repositories are recorded as gitlinks.
  - The tree comes from `write-tree`, and the temporary index is then renamed over the store
    index (up to 3 retries, 20 ms apart).
  - Fast path: the last tree is returned when the scanned index fingerprint is unchanged, no
    tracked file changed, and every untracked path is ignored.
  - Temporary indexes older than 1 h are swept, judged by the creation time in the file name.
  - An index that Git cannot read (judged by exit status) is rebuilt from the source index, and
    the capture is retried.
- **Diff**
  - Three concurrent commands: `--name-status -z`, `--numstat -z`, and the patch with
    `--unified=<context, default 3>`.
  - The patch budget is 10 MB in total. Truncated or missing chunks get `empty_patch`, and
    binary files get an empty patch.
  - Path selections are grouped to fit the argument budget (128 KiB, or 24 000 characters on
    Windows).
- **Restore**
  - Paths that can be batched are checked out per tree with
    `checkout <tree> --pathspec-from-file=- --pathspec-file-nul` against one temporary index.
    Paths absent from the tree are found with `ls-tree` and removed afterwards.
  - Other paths (non-ASCII, nested, or case-colliding) are restored one at a time with
    `ls-tree` + `checkout <tree> -- :(literal)<path>`.
- **Pack**
  - Runs only with at least 2048 loose objects or at least 16 local packs. Packs with `.keep`
    or `.promisor` files are not counted.
  - Requires the `opencode-pack.lock` file, created with `wx`; a lock older than 1 h is stale.
  - Sweeps `tmp_*` files and packs that have no index.
  - Runs `-c pack.threads=2 -c pack.windowMemory=64m pack-objects -q --non-empty` with an
    explicit list of object IDs. It checks that every listed object is in the new pack indexes
    before deleting anything, and never prunes.
- **Repository cache**
  - The checkout path is `<repos>/<host parts>/<segments>[@encodeURIComponent(branch)]`.
  - Runs under an `oc_util::flock` lock keyed `repository-cache:<localPath>` in the given lock
    root (normally `<state>/locks`).
  - Reuses a checkout only when it sits exactly at the cache path and has the same origin.
  - KV entry `{attemptedAt}` is written before any network work. `Refresh::Daily` waits 24 h
    between attempts.

## Not ported here (belongs to other crates)

- The `worktree`/`project` DB tables, `worktree.updated` events, and the project
  `commands.start` hook (oc-core).
- The `Vcs` service shell (provider registry, cached info, `vcs.branch.updated` events) and the
  Mercurial provider (`plugin/vcs/hg.ts`), which belong to oc-core.
- `Project.resolve` ID derivation (oc-core). It caches discovery by `.git` path plus
  `git::fingerprint`, and runs Git through `process::git_executable()`.

## Tests

`cargo test -p oc-git` runs the following against throwaway repositories in temp directories:

- unit tests for patch parsing, paths, pack index parsing, batching rules, reference parsing,
  slugs and retention
- `tests/git.rs`, ported from upstream `git.test.ts`
- `tests/vcs.rs`, covering the git VCS provider: status, no-HEAD repos, working/branch/committed
  diffs, review base, branches, and scoping to a subdirectory
- `tests/services.rs`, covering the snapshot, worktree strategy and repository cache scenarios
  from upstream `snapshot.test.ts` and `repository-cache.test.ts`
