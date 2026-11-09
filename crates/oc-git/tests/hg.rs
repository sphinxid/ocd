//! `packages/core/test/vcs-hg.test.ts`: the Mercurial provider against a real `hg`. Every test
//! but the first skips when `hg` is not on `PATH`.

mod common;

use std::path::Path;

use common::*;
use oc_git::HgVcs;
use oc_git::vcs::{DiffOptions, Mode, UntrackedMode, VcsFileStatus};
use oc_schema::types::FileStatus;

fn have_hg() -> bool {
    let found = std::process::Command::new("hg")
        .arg("--version")
        .env("HGPLAIN", "1")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok_and(|s| s.success());
    if !found {
        eprintln!("skipping: hg is not on PATH");
    }
    found
}

fn hg(directory: &Path, args: &[&str]) {
    let out = std::process::Command::new("hg").args(args).current_dir(directory).env("HGPLAIN", "1").output().unwrap();
    assert!(out.status.success(), "hg {args:?}: {}", String::from_utf8_lossy(&out.stderr));
}

fn commit_all(directory: &Path, message: &str) {
    hg(directory, &["addremove", "-q"]);
    hg(directory, &["commit", "-q", "-m", message, "-u", "test"]);
}

fn repo() -> (tempfile::TempDir, std::path::PathBuf) {
    let root = tmp();
    let directory = real(root.path());
    hg(&directory, &["init"]);
    (root, directory)
}

fn status(file: &str, additions: u64, deletions: u64, status: FileStatus) -> VcsFileStatus {
    VcsFileStatus { file: file.into(), additions, deletions, status }
}

fn names(rows: &[oc_schema::types::FileDiff]) -> Vec<(&str, FileStatus)> {
    rows.iter().map(|d| (d.file.as_str(), d.status)).collect()
}

#[tokio::test]
async fn rejects_unsupported_review_modes_and_bases_without_requiring_hg() {
    let root = tmp();
    let vcs = HgVcs::new(root.path(), root.path());
    let committed = vcs.diff(Mode::Committed, &DiffOptions::default()).await.unwrap_err();
    assert_eq!(committed.message, "The Mercurial provider does not support committed reviews or explicit bases");
    let based = vcs.diff(Mode::Branch, &DiffOptions { base: Some("release".into()), ..Default::default() }).await;
    assert_eq!(based.unwrap_err(), committed);
}

#[tokio::test]
async fn reports_modified_missing_and_untracked_files() {
    if !have_hg() {
        return;
    }
    let (_root, directory) = repo();
    write(directory.join("keep.txt"), "one\ntwo\n");
    write(directory.join("gone.txt"), "bye\n");
    commit_all(&directory, "initial");
    write(directory.join("keep.txt"), "one\nthree\n");
    std::fs::remove_file(directory.join("gone.txt")).unwrap();
    write(directory.join("new.txt"), "hello\nworld\n");
    let vcs = HgVcs::new(&directory, &directory);
    assert_eq!(
        vcs.status().await,
        [
            status("gone.txt", 0, 1, FileStatus::Deleted),
            status("keep.txt", 1, 1, FileStatus::Modified),
            status("new.txt", 2, 0, FileStatus::Added),
        ]
    );
}

#[tokio::test]
async fn diffs_the_working_copy_with_synthesized_untracked_and_missing_patches() {
    if !have_hg() {
        return;
    }
    let (_root, directory) = repo();
    write(directory.join("keep.txt"), "one\ntwo\n");
    write(directory.join("gone.txt"), "bye\n");
    commit_all(&directory, "initial");
    write(directory.join("keep.txt"), "one\nthree\n");
    std::fs::remove_file(directory.join("gone.txt")).unwrap();
    write(directory.join("spaced name.txt"), "hello\n");
    let vcs = HgVcs::new(&directory, &directory);
    let diff = vcs.diff(Mode::Working, &DiffOptions::default()).await.unwrap();
    assert_eq!(
        names(&diff),
        [("gone.txt", FileStatus::Deleted), ("keep.txt", FileStatus::Modified), ("spaced name.txt", FileStatus::Added)]
    );
    assert_eq!(diff[0].patch, oc_git::patch::delete_patch("gone.txt", "bye\n"));
    assert!(diff[1].patch.starts_with("diff --git a/keep.txt b/keep.txt\n"));
    assert!(diff[1].patch.contains("-two"));
    assert!(diff[1].patch.contains("+three"));
    assert_eq!((diff[1].additions, diff[1].deletions), (1, 1));
    assert_eq!(diff[2].patch, oc_git::patch::add_patch("spaced name.txt", "hello\n"));
    assert_eq!(diff[2].additions, 1);
    // The untracked option is ignored.
    let folders = DiffOptions { untracked: Some(UntrackedMode::Folders), ..Default::default() };
    assert_eq!(vcs.diff(Mode::Working, &folders).await.unwrap(), diff);
}

#[tokio::test]
async fn reports_the_branch() {
    if !have_hg() {
        return;
    }
    let (_root, directory) = repo();
    write(directory.join("file.txt"), "one\n");
    commit_all(&directory, "initial");
    let vcs = HgVcs::new(&directory, &directory);
    let info = serde_json::to_value(vcs.info().await).unwrap();
    assert_eq!(info, serde_json::json!({"branch": {"current": "default", "default": "default"}}));
    hg(&directory, &["branch", "-q", "feature"]);
    assert_eq!(vcs.info().await.branch.current.as_deref(), Some("feature"));
    assert!(vcs.branches().await.is_empty());
}

#[tokio::test]
async fn respects_the_context_option() {
    if !have_hg() {
        return;
    }
    let (_root, directory) = repo();
    let body: String = (0..20).map(|index| format!("line-{index}\n")).collect();
    write(directory.join("file.txt"), &body);
    commit_all(&directory, "initial");
    write(directory.join("file.txt"), body.replace("line-10", "changed"));
    let vcs = HgVcs::new(&directory, &directory);
    let full = vcs.diff(Mode::Working, &DiffOptions::default()).await.unwrap();
    assert!(full[0].patch.contains("line-0"));
    assert!(full[0].patch.contains("line-19"));
    let tight = vcs.diff(Mode::Working, &DiffOptions { context: Some(1), ..Default::default() }).await.unwrap();
    assert!(tight[0].patch.contains("line-9"));
    assert!(!tight[0].patch.contains("line-0"));
}

#[tokio::test]
async fn diffs_before_the_first_commit() {
    if !have_hg() {
        return;
    }
    let (_root, directory) = repo();
    write(directory.join("tracked.txt"), "a\nb\n");
    hg(&directory, &["add", "-q", "tracked.txt"]);
    write(directory.join("loose.txt"), "hello\n");
    let vcs = HgVcs::new(&directory, &directory);
    assert_eq!(vcs.status().await, [status("loose.txt", 1, 0, FileStatus::Added), status("tracked.txt", 2, 0, FileStatus::Added)]);
    let diff = vcs.diff(Mode::Working, &DiffOptions::default()).await.unwrap();
    assert_eq!(diff.len(), 2);
    assert!(diff[0].patch.contains("+hello"));
    assert!(diff[1].patch.contains("+a"));
}

#[tokio::test]
async fn diffs_a_named_branch_against_the_default_branch() {
    if !have_hg() {
        return;
    }
    let (_root, directory) = repo();
    write(directory.join("file.txt"), "one\n");
    commit_all(&directory, "initial");
    let vcs = HgVcs::new(&directory, &directory);
    assert!(vcs.diff(Mode::Branch, &DiffOptions::default()).await.unwrap().is_empty());
    hg(&directory, &["branch", "-q", "feature"]);
    write(directory.join("file.txt"), "one\ntwo\n");
    commit_all(&directory, "feature change");
    let diff = vcs.diff(Mode::Branch, &DiffOptions::default()).await.unwrap();
    assert_eq!(names(&diff), [("file.txt", FileStatus::Modified)]);
    assert!(diff[0].patch.contains("+two"));
}

#[tokio::test]
async fn scopes_to_the_location_directory_and_inits() {
    if !have_hg() {
        return;
    }
    let root = tmp();
    let directory = real(root.path());
    HgVcs::init(&directory).await.unwrap();
    assert!(directory.join(".hg").is_dir());
    write(directory.join("top.txt"), "top\n");
    write(directory.join("sub/inner.txt"), "inner\n");
    let vcs = HgVcs::new(directory.join("sub"), &directory);
    let diff = vcs.diff(Mode::Working, &DiffOptions::default()).await.unwrap();
    // `hg status` names files with the platform separator (`ui.slash` is off), and upstream keeps them that way.
    let inner = if cfg!(windows) { "sub\\inner.txt" } else { "sub/inner.txt" };
    assert_eq!(names(&diff), [(inner, FileStatus::Added)]);
    assert!(HgVcs::init(&directory.join("missing")).await.is_err());
}
