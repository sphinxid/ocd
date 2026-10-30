//! Captures traced with `GIT_TRACE`. The variable is process-wide, so both scenarios run in one
//! test of their own binary.

mod common;

use std::path::{Path, PathBuf};

use common::*;
use oc_git::{CaptureInput, Git, Repository};

struct Setup {
    _root: tempfile::TempDir,
    base: PathBuf,
    project: PathBuf,
    repository: Repository,
    source: Repository,
    git: Git,
}

async fn prepare() -> Setup {
    let root = tmp();
    let base = real(root.path());
    let project = base.join("project");
    init_repo(&project);
    write(project.join("a.txt"), "a\n");
    write(project.join("b.txt"), "b\n");
    commit_all(&project, "initial");
    let git = Git::new();
    let source = git.discover(&project).await.unwrap();
    let repository = git.create(&source.worktree, &base.join("storage"), Some(&source)).await.unwrap();
    Setup { _root: root, base, project, repository, source, git }
}

fn input(setup: &Setup, maximum: Option<u64>) -> CaptureInput {
    CaptureInput {
        repository: setup.repository.clone(),
        scopes: vec![".".to_string()],
        ignores: Some(setup.source.clone()),
        maximum_untracked_file_bytes: maximum,
    }
}

/// `traced`: the built-in commands a `GIT_TRACE` file recorded.
fn traced(file: &Path) -> Vec<String> {
    let text = std::fs::read_to_string(file).unwrap_or_default();
    let pattern = regex::Regex::new(r"trace: built-in: git (\S+)").unwrap();
    text.lines().filter_map(|line| pattern.captures(line).map(|c| c[1].to_string())).collect()
}

fn with_trace(file: &Path) {
    // SAFETY: this binary runs a single test, so no other thread reads the environment.
    unsafe { std::env::set_var("GIT_TRACE", file) };
}

fn without_trace() {
    // SAFETY: see `with_trace`.
    unsafe { std::env::remove_var("GIT_TRACE") };
}

#[tokio::test]
async fn traced_captures() {
    // git.test.ts "queued captures share one capture that still sees changes made before the call"
    let setup = prepare().await;
    let capture = input(&setup, None);
    let before = setup.git.capture(&capture).await.unwrap();
    write(setup.project.join("a.txt"), "changed\n");
    let trace = setup.base.join("trace");
    with_trace(&trace);
    let trees = futures::future::join_all((0..4).map(|_| setup.git.capture(&capture))).await;
    without_trace();
    let trees: Vec<_> = trees.into_iter().map(Result::unwrap).collect();
    assert!(trees.iter().all(|tree| *tree == trees[0]));
    assert_eq!(setup.git.tree_files(&setup.repository, &before, &trees[0]).await.unwrap(), vec!["a.txt".to_string()]);
    let listed = traced(&trace).into_iter().filter(|command| command == "ls-files").count();
    assert!(listed <= 2, "ls-files ran {listed} times");

    // git.test.ts "skips repeat ignore checks for oversized and info/exclude-only untracked files"
    let setup = prepare().await;
    write(setup.project.join(".git/info/exclude"), "excluded.txt\n");
    write(setup.project.join("excluded.txt"), "x\n");
    write(setup.project.join("large.txt"), "too large\n");
    let capture = input(&setup, Some(4));
    let first = setup.git.capture(&capture).await.unwrap();
    let trace = setup.base.join("trace");
    with_trace(&trace);
    let second = setup.git.capture(&capture).await.unwrap();
    without_trace();
    assert_eq!(second, first);
    let mut commands = traced(&trace);
    commands.sort();
    assert_eq!(commands, vec!["diff-files".to_string(), "ls-files".to_string()]);
    let storage = setup.base.join("storage");
    let entries = git(&setup.base, &["--git-dir", storage.to_str().unwrap(), "ls-tree", "--name-only", first.as_str()]);
    assert_eq!(entries.lines().filter(|line| !line.is_empty()).collect::<Vec<_>>(), vec!["a.txt", "b.txt"]);
}
