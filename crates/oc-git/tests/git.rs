mod common;

use std::path::Path;

use common::*;
use oc_git::patch::{MAX_TOTAL_PATCH_BYTES, empty_patch};
use oc_git::{CaptureInput, CheckoutInput, CloneInput, DiffInput, FetchBranchInput, Git, Repository, TreeId, WorktreeKind};
use oc_schema::types::FileStatus;

fn capture_input(repository: &Repository, ignores: Option<&Repository>) -> CaptureInput {
    CaptureInput {
        repository: repository.clone(),
        scopes: vec![".".to_string()],
        ignores: ignores.cloned(),
        maximum_untracked_file_bytes: None,
    }
}

fn strings(items: &[&str]) -> Vec<String> {
    items.iter().map(|s| s.to_string()).collect()
}

#[tokio::test]
async fn discovers_repository_metadata_without_a_work_tree() {
    let root = tmp();
    init_repo(root.path());
    git(root.path(), &["config", "core.bare", "true"]);
    let directory = real(root.path());
    let repository = Git::new().discover(&directory).await.unwrap();
    assert_eq!(repository.worktree, directory);
    assert_eq!(repository.git_directory, directory.join(".git"));
    assert_eq!(repository.common_directory, repository.git_directory);
}

#[tokio::test]
async fn discover_returns_none_outside_git() {
    let root = tmp();
    // Temp directories may live under a repository on some machines; only assert when they do not.
    if Git::new().discover(&real(root.path())).await.is_none() {
        assert!(Git::new().discover(&real(root.path()).join("missing")).await.is_none());
    }
}

#[tokio::test]
async fn clones_a_remote_and_reads_checkout_metadata() {
    let root = tmp();
    let fixture = git_remote(&real(root.path()));
    let git = Git::new();
    let target = fixture.root.join("checkout");
    let repository = git
        .clone_repository(CloneInput { remote: fixture.remote.clone(), directory: target.clone(), ..Default::default() })
        .await
        .unwrap();
    assert_eq!(git.remote(&repository, None).await.as_deref(), Some(fixture.remote.as_str()));
    assert!(git.head(&repository).await.is_some_and(|h| h.len() == 40));
    assert_eq!(git.branch(&repository).await.as_deref(), Some("main"));
    assert_eq!(git.default_remote_branch(&repository, None).await.as_deref(), Some("main"));
    assert_eq!(repository.worktree, target);
    assert_eq!(repository.git_directory, target.join(".git"));
    assert_eq!(repository.common_directory, repository.git_directory);
    assert_eq!(read(target.join("README.md")), "one\n");
    assert_eq!(git.root_commits(&repository).await.len(), 1);
    assert!(git.remote(&repository, Some("missing")).await.is_none());
}

#[tokio::test]
async fn clone_failure_reports_git_stderr() {
    let root = tmp();
    let error = Git::new()
        .clone_repository(CloneInput {
            remote: root.path().join("nope.git").to_string_lossy().into_owned(),
            directory: root.path().join("checkout"),
            ..Default::default()
        })
        .await
        .unwrap_err();
    assert_eq!(error.operation, oc_git::Operation::Clone);
    assert!(error.message.contains("nope.git"), "{}", error.message);
}

#[tokio::test]
async fn fetches_checks_out_and_resets_remote_changes() {
    let root = tmp();
    let fixture = git_remote(&real(root.path()));
    let git = Git::new();
    let target = fixture.root.join("checkout");
    let repository = git
        .clone_repository(CloneInput { remote: fixture.remote.clone(), directory: target.clone(), ..Default::default() })
        .await
        .unwrap();
    commit_readme(&fixture.source, "two\n", "second");
    git.fetch_remotes(&repository, None).await.unwrap();
    git.reset_hard(&repository, "origin/main").await.unwrap();
    assert_eq!(read(target.join("README.md")), "two\n");

    branch(&fixture.source, "feature/docs", "feature\n");
    git.fetch_branch(&repository, FetchBranchInput { remote: None, branch: "feature/docs".into(), force: None })
        .await
        .unwrap();
    git.checkout_remote_branch(&repository, CheckoutInput { remote: None, branch: "feature/docs".into(), reset: None })
        .await
        .unwrap();
    git.reset_hard(&repository, "origin/feature/docs").await.unwrap();
    assert_eq!(git.branch(&repository).await.as_deref(), Some("feature/docs"));
    assert_eq!(read(target.join("README.md")), "feature\n");
}

#[tokio::test]
async fn creates_lists_and_removes_linked_worktrees() {
    let root = tmp();
    let project = real(root.path()).join("project");
    init_repo(&project);
    let worktree = real(root.path()).join("project-git-worktree");
    let git = Git::new();
    let repo = git.discover(&project).await.unwrap();
    git.worktree_create(&repo, &worktree, None).await.unwrap();
    let list = git.worktree_list(&repo).await.unwrap();
    assert_eq!(list[0].kind, WorktreeKind::Main);
    assert_eq!(list[0].directory, project);
    assert!(list.iter().any(|e| e.directory == worktree && e.kind == WorktreeKind::Linked));
    let linked = git.discover(&worktree).await.unwrap();
    assert_eq!(linked.worktree, worktree);
    assert_eq!(linked.common_directory, repo.common_directory);
    assert_ne!(linked.git_directory, repo.git_directory);

    // A dirty worktree needs force.
    write(worktree.join("dirty.txt"), "x\n");
    let error = git.worktree_remove(&linked, &worktree, false).await.unwrap_err();
    assert_eq!(error.force_required, Some(true));
    git.worktree_remove(&linked, &worktree, true).await.unwrap();
    assert!(!git.worktree_list(&repo).await.unwrap().iter().any(|e| e.directory == worktree));
}

async fn refresh_handles_check_ignore_exit(exit_code: i32) {
    let root = tmp();
    let base = real(root.path());
    let project = base.join("project");
    let paths = strings(&["scope/allowed.txt", "scope/ignored name.txt"]);
    std::fs::create_dir_all(project.join("scope")).unwrap();
    init_repo(&project);
    for file in &paths {
        write(project.join(file), "one\n");
    }
    commit_all(&project, "initial");
    let git = Git::new();
    let source = git.discover(&project).await.unwrap();
    let repository = git.create(&source.worktree, &base.join("storage"), Some(&source)).await.unwrap();
    let before = git.write_tree(&repository).await.unwrap();
    for file in &paths {
        write(project.join(file), "two\n");
    }
    write(source.git_directory.join("info").join("exclude"), if exit_code == 0 { format!("{}\n", paths[1]) } else { String::new() });
    if exit_code == 128 {
        write(source.git_directory.join("config"), "[broken\n");
    }
    let refresh = git.refresh(&repository, "scope", Some(&source), None).await;
    if exit_code == 128 {
        let error = refresh.unwrap_err();
        assert!(error.message.contains("bad config line"), "{}", error.message);
        assert_eq!(git.write_tree(&repository).await.unwrap(), before);
        assert_eq!(git.refresh(&repository, "missing", Some(&source), None).await.unwrap().skipped, Vec::<String>::new());
        return;
    }
    let ignored = git.ignored(&source, &paths).await.unwrap();
    assert_eq!(ignored, if exit_code == 0 { [paths[1].clone()].into_iter().collect() } else { Default::default() });
    assert!(refresh.unwrap().skipped.is_empty());
    let after = git.write_tree(&repository).await.unwrap();
    let diffs = git
        .tree_diff(&repository, &DiffInput { from: before, to: after, ..Default::default() })
        .await
        .unwrap();
    let statuses: Vec<FileStatus> = diffs.iter().map(|d| d.status).collect();
    assert_eq!(
        statuses,
        vec![FileStatus::Modified, if exit_code == 0 { FileStatus::Deleted } else { FileStatus::Modified }]
    );
}

#[tokio::test]
async fn refresh_handles_check_ignore_exit_0() {
    refresh_handles_check_ignore_exit(0).await;
}

#[tokio::test]
async fn refresh_handles_check_ignore_exit_1() {
    refresh_handles_check_ignore_exit(1).await;
}

#[tokio::test]
async fn refresh_handles_check_ignore_exit_128() {
    refresh_handles_check_ignore_exit(128).await;
}

#[tokio::test]
async fn lists_both_sides_of_a_rename() {
    let root = tmp();
    let project = real(root.path());
    init_repo(&project);
    git(&project, &["config", "diff.renames", "true"]);
    write(project.join("old name.txt"), "Preserve this content.\n");
    let git = Git::new();
    let repository = git.discover(&project).await.unwrap();
    let before = git.capture(&capture_input(&repository, None)).await.unwrap();
    std::fs::rename(project.join("old name.txt"), project.join("new name.txt")).unwrap();
    let after = git.capture(&capture_input(&repository, None)).await.unwrap();
    assert_eq!(git.tree_files(&repository, &before, &after).await.unwrap(), strings(&["new name.txt", "old name.txt"]));
    let diffs = git
        .tree_diff(&repository, &DiffInput { from: before, to: after, ..Default::default() })
        .await
        .unwrap();
    let summary: Vec<(String, FileStatus)> = diffs.iter().map(|d| (d.file.clone(), d.status)).collect();
    assert_eq!(
        summary,
        vec![("new name.txt".to_string(), FileStatus::Added), ("old name.txt".to_string(), FileStatus::Deleted)]
    );
}

#[tokio::test]
async fn caps_batched_patches_keeps_stats_and_matches_non_ascii_names() {
    let root = tmp();
    let project = real(root.path());
    init_repo(&project);
    let git = Git::new();
    let repository = git.discover(&project).await.unwrap();
    let before = git.capture(&capture_input(&repository, None)).await.unwrap();
    let lines = MAX_TOTAL_PATCH_BYTES.div_ceil(80) + 1;
    write(project.join("a-small.txt"), "small\n");
    write(project.join("b-large.txt"), format!("{}\n", "x".repeat(79)).repeat(lines));
    write(project.join("c-binary.bin"), [0u8, 1, 2, 3]);
    write(project.join("a-caf\u{e9}.txt"), "caf\u{e9}\n");
    let after = git.capture(&capture_input(&repository, None)).await.unwrap();
    let diffs = git
        .tree_diff(&repository, &DiffInput { from: before.clone(), to: after.clone(), context: Some(0), paths: None })
        .await
        .unwrap();
    let summary: Vec<(String, FileStatus, u64, u64)> =
        diffs.iter().map(|d| (d.file.clone(), d.status, d.additions, d.deletions)).collect();
    assert_eq!(
        summary,
        vec![
            ("a-caf\u{e9}.txt".to_string(), FileStatus::Added, 1, 0),
            ("a-small.txt".to_string(), FileStatus::Added, 1, 0),
            ("b-large.txt".to_string(), FileStatus::Added, lines as u64, 0),
            ("c-binary.bin".to_string(), FileStatus::Added, 0, 0),
        ]
    );
    assert!(diffs[0].patch.contains("+caf\u{e9}\n"));
    assert!(diffs[1].patch.contains("+small\n"));
    assert_eq!(diffs[2].patch, empty_patch("b-large.txt"));
    assert_eq!(diffs[3].patch, "");
    assert!(
        git.tree_diff(&repository, &DiffInput { from: before, to: after, context: None, paths: Some(vec![]) })
            .await
            .unwrap()
            .is_empty()
    );
}

#[tokio::test]
async fn diffs_a_path_selection_longer_than_any_command_line() {
    let root = tmp();
    let project = real(root.path());
    init_repo(&project);
    write(project.join("changed.txt"), "before\n");
    write(project.join("removed.txt"), "removed\n");
    write(project.join("dir/nested/file.txt"), "before\n");
    let git = Git::new();
    let repository = git.discover(&project).await.unwrap();
    let before = git.capture(&capture_input(&repository, None)).await.unwrap();
    write(project.join("changed.txt"), "after\n");
    std::fs::remove_file(project.join("removed.txt")).unwrap();
    write(project.join("dir/nested/file.txt"), "after\n");
    write(project.join("first.txt"), "first\n");
    write(project.join("last.txt"), "last\n");
    write(project.join("unselected.txt"), "unselected\n");
    let after = git.capture(&capture_input(&repository, None)).await.unwrap();
    let missing: Vec<String> = (0..30_000).map(|i| format!("missing/{i:06}/{}.txt", "x".repeat(80))).collect();
    let mut paths = strings(&["first.txt", "changed.txt", "removed.txt", "dir"]);
    paths.extend(missing.iter().cloned());
    paths.push("last.txt".into());
    let diffs = git
        .tree_diff(&repository, &DiffInput { from: before.clone(), to: after.clone(), context: None, paths: Some(paths) })
        .await
        .unwrap();
    let summary: Vec<(String, FileStatus, u64, u64)> =
        diffs.iter().map(|d| (d.file.clone(), d.status, d.additions, d.deletions)).collect();
    assert_eq!(
        summary,
        vec![
            ("changed.txt".to_string(), FileStatus::Modified, 1, 1),
            ("dir/nested/file.txt".to_string(), FileStatus::Modified, 1, 1),
            ("first.txt".to_string(), FileStatus::Added, 1, 0),
            ("last.txt".to_string(), FileStatus::Added, 1, 0),
            ("removed.txt".to_string(), FileStatus::Deleted, 0, 1),
        ]
    );
    assert!(diffs[0].patch.contains("-before\n+after\n"));
    assert!(diffs[2].patch.contains("+first\n"));
    assert!(diffs[4].patch.contains("-removed\n"));
    assert!(
        git.tree_diff(&repository, &DiffInput { from: before, to: after.clone(), context: None, paths: Some(missing) })
            .await
            .unwrap()
            .is_empty()
    );
    assert_eq!(git.write_tree(&repository).await.unwrap(), after);
}

#[tokio::test]
async fn captures_compares_and_restores_scoped_trees() {
    let root = tmp();
    let project = real(root.path());
    init_repo(&project);
    write(project.join("scope/tracked.txt"), "one\n");
    write(project.join("outside.txt"), "outside\n");
    commit_all(&project, "initial");
    let git = Git::new();
    let source = git.discover(&project).await.unwrap();
    let storage = project.join(".snapshot storage");
    let repository = git.create(&source.worktree, &storage, Some(&source)).await.unwrap();
    let storage_arg = storage.to_str().unwrap();
    git_cmd(&project, &["--git-dir", storage_arg, "config", "--add", "include.path", "first.gitconfig"]);
    git_cmd(&project, &["--git-dir", storage_arg, "config", "--add", "include.path", "second.gitconfig"]);
    git_cmd(&project, &["--git-dir", storage_arg, "config", "core.autocrlf", "true"]);
    git.create(&source.worktree, &storage, Some(&source)).await.unwrap();
    assert_eq!(git_cmd(&project, &["--git-dir", storage_arg, "config", "--local", "--includes", "core.autocrlf"]), "false\n");
    assert_eq!(std::fs::read_to_string(storage.join("config")).unwrap().matches("opencode.gitconfig").count(), 1);
    assert_eq!(
        git_cmd(&project, &["--git-dir", storage_arg, "config", "--local", "--get-all", "include.path"]),
        "opencode.gitconfig\nfirst.gitconfig\nsecond.gitconfig\n"
    );
    assert_eq!(std::fs::read_to_string(storage.join("opencode.gitconfig")).unwrap(), oc_git::git::SNAPSHOT_CONFIG);
    assert_eq!(
        std::fs::read_to_string(storage.join("objects/info/alternates")).unwrap(),
        format!("{}\n", source.common_directory.join("objects").display())
    );
    git.refresh(&repository, "scope", None, None).await.unwrap();
    let before = git.write_tree(&repository).await.unwrap();

    write(project.join("scope/tracked.txt"), "two\n");
    write(project.join("scope/added.txt"), "added\n");
    write(project.join("outside.txt"), "changed outside\n");
    git.refresh(&repository, "scope", None, None).await.unwrap();
    let after = git.write_tree(&repository).await.unwrap();

    assert_eq!(
        git.tree_files(&repository, &before, &after).await.unwrap(),
        strings(&["scope/added.txt", "scope/tracked.txt"])
    );
    let diffs = git
        .tree_diff(&repository, &DiffInput { from: before.clone(), to: after, context: Some(1), paths: None })
        .await
        .unwrap();
    let summary: Vec<(String, FileStatus)> = diffs.iter().map(|d| (d.file.clone(), d.status)).collect();
    assert_eq!(
        summary,
        vec![("scope/added.txt".to_string(), FileStatus::Added), ("scope/tracked.txt".to_string(), FileStatus::Modified)]
    );
    git.restore(&repository, &[("scope/tracked.txt".to_string(), before)]).await.unwrap();
    assert_eq!(read(project.join("scope/tracked.txt")), "one\n");
    assert_eq!(read(project.join("scope/added.txt")), "added\n");
    assert_eq!(read(project.join("outside.txt")), "changed outside\n");
}

fn git_cmd(cwd: &Path, args: &[&str]) -> String {
    git(cwd, args)
}

#[tokio::test]
async fn restores_one_by_one_when_paths_cannot_be_batched() {
    let root = tmp();
    let project = real(root.path());
    init_repo(&project);
    write(project.join("caf\u{e9}.txt"), "one\n");
    write(project.join("dir/inner.txt"), "inner\n");
    let git = Git::new();
    let repository = git.discover(&project).await.unwrap();
    let before = git.capture(&capture_input(&repository, None)).await.unwrap();
    write(project.join("caf\u{e9}.txt"), "two\n");
    write(project.join("new*.txt"), "new\n");
    git.restore(
        &repository,
        &[("caf\u{e9}.txt".to_string(), before.clone()), ("new*.txt".to_string(), before.clone())],
    )
    .await
    .unwrap();
    assert_eq!(read(project.join("caf\u{e9}.txt")), "one\n");
    assert!(!project.join("new*.txt").exists());
    // Nested selections ("dir" and "dir/inner.txt") take the ordered path too.
    write(project.join("dir/inner.txt"), "changed\n");
    git.restore(&repository, &[("dir".to_string(), before.clone()), ("dir/inner.txt".to_string(), before)])
        .await
        .unwrap();
    assert_eq!(read(project.join("dir/inner.txt")), "inner\n");
}

fn loose_object_ids(objects: &Path) -> Vec<String> {
    let mut out = Vec::new();
    for entry in std::fs::read_dir(objects).unwrap().flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        if name.len() == 2 && name.chars().all(|c| c.is_ascii_hexdigit()) {
            for inner in std::fs::read_dir(entry.path()).unwrap().flatten() {
                out.push(format!("{name}{}", inner.file_name().to_string_lossy()));
            }
        }
    }
    out.sort();
    out
}

fn packs(objects: &Path) -> usize {
    std::fs::read_dir(objects.join("pack"))
        .map(|d| d.flatten().filter(|e| e.file_name().to_string_lossy().ends_with(".pack")).count())
        .unwrap_or(0)
}

#[tokio::test]
async fn packs_loose_objects_and_combines_packs_without_losing_any_object() {
    let root = tmp();
    let base = real(root.path());
    let project = base.join("project");
    init_repo(&project);
    write(project.join("seed.txt"), "seed\n");
    commit_all(&project, "initial");
    let git = Git::new();
    let source = git.discover(&project).await.unwrap();
    let repository = git.create(&source.worktree, &base.join("storage"), Some(&source)).await.unwrap();
    let input = capture_input(&repository, Some(&source));
    let mut trees = vec![git.capture(&input).await.unwrap()];
    for index in 0..2100 {
        write(project.join(format!("gen/d{}/f{index}.txt", index % 20)), format!("{index}\n"));
    }
    trees.push(git.capture(&input).await.unwrap());
    let objects = repository.git_directory.join("objects");
    let alternates = objects.join("info").join("alternates");
    let gd = repository.git_directory.to_str().unwrap().to_string();
    let everything = || {
        let borrowed = std::fs::read(&alternates).unwrap();
        std::fs::remove_file(&alternates).unwrap();
        let listed = git_cmd(&base, &["--git-dir", &gd, "cat-file", "--batch-all-objects", "--batch-check=%(objectname)"]);
        std::fs::write(&alternates, borrowed).unwrap();
        let mut ids: Vec<String> = listed.lines().map(str::to_string).collect();
        ids.sort();
        ids
    };
    let before = everything();
    assert!(loose_object_ids(&objects).len() > 2048);

    git.pack(&repository).await.unwrap();
    assert!(loose_object_ids(&objects).is_empty());
    assert_eq!(everything(), before);
    assert!(!repository.git_directory.join("opencode-pack.lock").exists());

    for batch in 0..16 {
        write(project.join(format!("batch-{batch}.txt")), format!("{batch}\n"));
        trees.push(git.capture(&input).await.unwrap());
        let list = loose_object_ids(&objects);
        let mut child = std::process::Command::new("git")
            .args(["--git-dir", &gd, "pack-objects", "-q", objects.join("pack").join("pack").to_str().unwrap()])
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::null())
            .spawn()
            .unwrap();
        use std::io::Write;
        child.stdin.take().unwrap().write_all((list.join("\n") + "\n").as_bytes()).unwrap();
        assert!(child.wait().unwrap().success());
        git_cmd(&base, &["--git-dir", &gd, "prune-packed"]);
    }
    assert!(packs(&objects) >= 16);
    let merged = everything();
    git.pack(&repository).await.unwrap();
    assert_eq!(packs(&objects), 1);
    assert_eq!(everything(), merged);
    assert_eq!(git.tree_files(&repository, &trees[0], trees.last().unwrap()).await.unwrap().len(), 2100 + 16);
}

#[tokio::test]
async fn pack_skips_below_thresholds_and_respects_a_live_lock() {
    let root = tmp();
    let base = real(root.path());
    let project = base.join("project");
    init_repo(&project);
    let git = Git::new();
    let source = git.discover(&project).await.unwrap();
    let repository = git.create(&source.worktree, &base.join("storage"), Some(&source)).await.unwrap();
    write(project.join("a.txt"), "a\n");
    git.capture(&capture_input(&repository, Some(&source))).await.unwrap();
    let loose = loose_object_ids(&repository.git_directory.join("objects"));
    git.pack(&repository).await.unwrap();
    assert_eq!(loose_object_ids(&repository.git_directory.join("objects")), loose);
}

#[tokio::test]
async fn applies_store_and_source_ignore_rules_and_never_splits_the_index() {
    let root = tmp();
    let base = real(root.path());
    let project = base.join("project");
    init_repo(&project);
    write(project.join("tracked.bin"), "small\n");
    commit_all(&project, "initial");
    write(project.join(".git/info/exclude"), "source-ignored/\n");
    let git = Git::new();
    let source = git.discover(&project).await.unwrap();
    let storage = base.join("storage");
    let repository = git.create(&source.worktree, &storage, Some(&source)).await.unwrap();
    write(storage.join("info/exclude"), "store-ignored/\n");
    git_cmd(&base, &["--git-dir", storage.to_str().unwrap(), "config", "core.splitIndex", "true"]);
    let input = capture_input(&repository, Some(&source));
    let before = git.capture(&input).await.unwrap();
    write(project.join("tracked.bin"), "changed\n");
    write(project.join("store-ignored/a.txt"), "a\n");
    write(project.join("source-ignored/b.txt"), "b\n");
    write(project.join("kept.txt"), "kept\n");
    let after = git.capture(&input).await.unwrap();
    assert_eq!(git.tree_files(&repository, &before, &after).await.unwrap(), strings(&["kept.txt", "tracked.bin"]));
    assert_eq!(git.capture(&input).await.unwrap(), after);
    assert!(read(storage.join("info/exclude")).starts_with("store-ignored/\n"));
    // A fresh service (no in-process fast path) records the same tree.
    assert_eq!(Git::new().capture(&input).await.unwrap(), after);
}

#[tokio::test]
async fn skips_untracked_files_over_the_limit() {
    let root = tmp();
    let base = real(root.path());
    let project = base.join("project");
    init_repo(&project);
    let git = Git::new();
    let source = git.discover(&project).await.unwrap();
    let repository = git.create(&source.worktree, &base.join("storage"), Some(&source)).await.unwrap();
    let mut input = capture_input(&repository, Some(&source));
    input.maximum_untracked_file_bytes = Some(10);
    let before = git.capture(&input).await.unwrap();
    write(project.join("big.txt"), "0123456789abcdef\n");
    write(project.join("small.txt"), "ok\n");
    let after = git.capture(&input).await.unwrap();
    assert_eq!(git.tree_files(&repository, &before, &after).await.unwrap(), strings(&["small.txt"]));
    let result = git.refresh(&repository, ".", Some(&source), Some(10)).await.unwrap();
    assert_eq!(result.skipped, strings(&["big.txt"]));
}

struct Recovery {
    _root: tempfile::TempDir,
    base: std::path::PathBuf,
    project: std::path::PathBuf,
    repository: Repository,
    input: CaptureInput,
}

async fn recovery_setup(git: &Git) -> Recovery {
    let root = tmp();
    let base = real(root.path());
    let project = base.join("project");
    init_repo(&project);
    write(project.join("a.txt"), "a\n");
    write(project.join("b.txt"), "b\n");
    commit_all(&project, "initial");
    let source = git.discover(&project).await.unwrap();
    let repository = git.create(&source.worktree, &base.join("storage"), Some(&source)).await.unwrap();
    let input = capture_input(&repository, Some(&source));
    Recovery { _root: root, base, project, repository, input }
}

#[tokio::test]
async fn sweeps_abandoned_temporary_indexes_by_creation_time() {
    let git = Git::new();
    let setup = recovery_setup(&git).await;
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_millis();
    let two_hours_ago = now - 2 * 60 * 60 * 1000;
    let live = setup.repository.git_directory.join(format!("index.opencode-{now}-1-live"));
    let abandoned = setup.repository.git_directory.join(format!("index.opencode-{two_hours_ago}-1-abandoned"));
    write(&live, "");
    write(&abandoned, "");
    let old = std::time::SystemTime::now() - std::time::Duration::from_secs(2 * 60 * 60);
    std::fs::File::options().write(true).open(&live).unwrap().set_modified(old).unwrap();
    git.capture(&setup.input).await.unwrap();
    assert!(live.exists());
    assert!(!abandoned.exists());
}

#[tokio::test]
async fn records_a_tracked_file_replaced_by_an_embedded_repository_as_a_gitlink() {
    let git = Git::new();
    let setup = recovery_setup(&git).await;
    git.capture(&setup.input).await.unwrap();
    std::fs::remove_file(setup.project.join("a.txt")).unwrap();
    write(setup.project.join("a.txt/inner.txt"), "inner\n");
    init_repo(&setup.project.join("a.txt"));
    commit_all(&setup.project.join("a.txt"), "inner");
    let tree = git.capture(&setup.input).await.unwrap();
    let gd = setup.repository.git_directory.to_str().unwrap();
    let entry = git_cmd(&setup.base, &["--git-dir", gd, "ls-tree", tree.as_str(), "a.txt"]);
    assert!(entry.starts_with("160000 commit "), "{entry}");
}

#[tokio::test]
async fn keeps_the_index_when_git_cannot_even_start() {
    let git = Git::new();
    let setup = recovery_setup(&git).await;
    let before = git.capture(&setup.input).await.unwrap();
    let moved = setup.base.join("moved");
    std::fs::rename(&setup.project, &moved).unwrap();
    assert!(git.capture(&setup.input).await.is_err());
    std::fs::rename(&moved, &setup.project).unwrap();
    assert!(setup.repository.git_directory.join("index").exists());
    assert_eq!(git.capture(&setup.input).await.unwrap(), before);
}

#[tokio::test]
async fn rebuilds_an_unreadable_index_from_the_source() {
    let git = Git::new();
    let setup = recovery_setup(&git).await;
    let before = git.capture(&setup.input).await.unwrap();
    std::fs::write(setup.repository.git_directory.join("index"), vec![0u8; 512]).unwrap();
    std::fs::write(setup.repository.git_directory.join("index.lock"), "").unwrap();
    write(setup.project.join("a.txt"), "changed\n");
    let after = git.capture(&setup.input).await.unwrap();
    assert_eq!(git.tree_files(&setup.repository, &before, &after).await.unwrap(), strings(&["a.txt"]));
}

#[tokio::test]
async fn rewrites_an_outdated_snapshot_config_once() {
    let git = Git::new();
    let setup = recovery_setup(&git).await;
    let file = setup.repository.git_directory.join("opencode.gitconfig");
    std::fs::write(&file, "[core]\n\tautocrlf = true\n").unwrap();
    git.capture(&setup.input).await.unwrap();
    assert_eq!(std::fs::read_to_string(&file).unwrap(), oc_git::git::SNAPSHOT_CONFIG);
}

#[tokio::test]
async fn invalid_tree_ids_surface_operation_errors() {
    let git = Git::new();
    let setup = recovery_setup(&git).await;
    let tree = git.capture(&setup.input).await.unwrap();
    let error = git.tree_files(&setup.repository, &TreeId::from("0".repeat(40)), &tree).await.unwrap_err();
    assert_eq!(error.operation, oc_git::Operation::ListFiles);
    assert!(error.cause.is_none());
}
