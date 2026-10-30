mod common;

use common::*;
use oc_git::vcs::{BaseSource, BranchOptions, DiffOptions, GitVcs, Mode, UntrackedMode};
use oc_schema::types::FileStatus;

fn summary(rows: &[oc_schema::types::FileDiff]) -> Vec<(String, FileStatus, u64, u64)> {
    rows.iter().map(|d| (d.file.clone(), d.status, d.additions, d.deletions)).collect()
}

fn row(file: &str, status: FileStatus, additions: u64, deletions: u64) -> (String, FileStatus, u64, u64) {
    (file.to_string(), status, additions, deletions)
}

fn project() -> (tempfile::TempDir, std::path::PathBuf) {
    let root = tmp();
    let project = real(root.path()).join("project");
    init_repo(&project);
    git(&project, &["branch", "-M", "main"]);
    write(project.join("tracked.txt"), "one\ntwo\n");
    write(project.join("gone.txt"), "bye\n");
    commit_all(&project, "initial");
    (root, project)
}

#[test]
fn adapter_futures_are_send() {
    fn assert_send<T: Send>(_: &T) {}
    let vcs = GitVcs::new("/x", "/x");
    assert_send(&vcs.status());
    assert_send(&vcs.diff(Mode::Working, &DiffOptions::default()));
    assert_send(&vcs.base());
    assert_send(&vcs.info());
}

#[tokio::test]
async fn status_reports_changes_with_line_counts() {
    let (_root, project) = project();
    write(project.join("tracked.txt"), "one\nTWO\nthree\n");
    std::fs::remove_file(project.join("gone.txt")).unwrap();
    write(project.join("new a.txt"), "a\nb\n");
    write(project.join("new[1].txt"), "x\n");
    write(project.join("Upper.txt"), "u\n");
    let vcs = GitVcs::new(&project, &project);
    let status = vcs.status().await;
    let rows: Vec<(String, FileStatus, u64, u64)> =
        status.iter().map(|s| (s.file.clone(), s.status, s.additions, s.deletions)).collect();
    assert_eq!(
        rows,
        vec![
            row("gone.txt", FileStatus::Deleted, 0, 1),
            row("new a.txt", FileStatus::Added, 2, 0),
            row("new[1].txt", FileStatus::Added, 1, 0),
            row("tracked.txt", FileStatus::Modified, 2, 1),
            row("Upper.txt", FileStatus::Added, 1, 0),
        ]
    );
}

#[tokio::test]
async fn status_and_diff_without_head() {
    let root = tmp();
    let project = real(root.path()).join("fresh");
    std::fs::create_dir_all(&project).unwrap();
    GitVcs::init(&project).await.unwrap();
    write(project.join("a.txt"), "a\n");
    let vcs = GitVcs::new(&project, &project);
    assert!(!vcs.has_head().await);
    assert_eq!(vcs.base().await.unwrap(), None);
    let status = vcs.status().await;
    assert_eq!(status.len(), 1);
    assert_eq!((status[0].status, status[0].additions), (FileStatus::Added, 1));
    let working = vcs.diff(Mode::Working, &DiffOptions::default()).await.unwrap();
    assert_eq!(summary(&working), vec![row("a.txt", FileStatus::Added, 1, 0)]);
    assert!(working[0].patch.contains("+a\n"));
    assert!(vcs.diff(Mode::Committed, &DiffOptions::default()).await.unwrap().is_empty());
    assert_eq!(summary(&vcs.diff(Mode::Branch, &DiffOptions::default()).await.unwrap()), summary(&working));
}

#[tokio::test]
async fn init_fails_with_upstream_message() {
    let root = tmp();
    let error = GitVcs::init(&root.path().join("missing")).await.unwrap_err();
    assert_eq!(error.message, "Git initialization failed");
}

#[tokio::test]
async fn working_diff_includes_untracked_files_and_matches_per_file_patches() {
    let (_root, project) = project();
    write(project.join("tracked.txt"), "one\nTWO\n");
    write(project.join("u1.txt"), "first\n");
    write(project.join("u2.txt"), "second\nline\n");
    write(project.join("bin.dat"), [0u8, 1, 2]);
    let vcs = GitVcs::new(&project, &project);
    let diffs = vcs.review_diff(Mode::Working, &DiffOptions { context: Some(0), ..Default::default() }).await.unwrap();
    assert_eq!(
        summary(&diffs),
        vec![
            row("bin.dat", FileStatus::Added, 0, 0),
            row("tracked.txt", FileStatus::Modified, 1, 1),
            row("u1.txt", FileStatus::Added, 1, 0),
            row("u2.txt", FileStatus::Added, 2, 0),
        ]
    );
    assert!(diffs[1].patch.starts_with("diff --git a/tracked.txt b/tracked.txt\n"));
    assert!(diffs[1].patch.contains("-two\n+TWO\n"));
    assert!(diffs[2].patch.contains("new file mode"));
    assert!(diffs[2].patch.contains("+first\n"));
    assert!(diffs[3].patch.contains("+second\n+line\n"));
    assert!(diffs[0].patch.contains("Binary files"));

    // A lone untracked file takes the per-file `--no-index` path; its patch matches the batch's text.
    let batch = diffs[2].patch.clone();
    std::fs::remove_file(project.join("u2.txt")).unwrap();
    std::fs::remove_file(project.join("bin.dat")).unwrap();
    let single = vcs.review_diff(Mode::Working, &DiffOptions { context: Some(0), ..Default::default() }).await.unwrap();
    let u1 = single.iter().find(|d| d.file == "u1.txt").unwrap();
    assert!(u1.patch.contains("+first\n"));
    assert_eq!(u1.patch.lines().filter(|l| l.starts_with('+') || l.starts_with('-')).count(),
        batch.lines().filter(|l| l.starts_with('+') || l.starts_with('-')).count());
}

#[tokio::test]
async fn branch_and_committed_diffs_use_the_merge_base_with_the_default_branch() {
    let (_root, project) = project();
    git(&project, &["checkout", "-q", "-b", "feature"]);
    write(project.join("feature.txt"), "feature\n");
    commit_all(&project, "feature work");
    write(project.join("tracked.txt"), "one\nchanged\n");
    write(project.join("scratch.txt"), "scratch\n");
    // Advance main after branching; the merge base keeps it out of the review.
    git(&project, &["checkout", "-q", "main"]);
    write(project.join("main-only.txt"), "main\n");
    git(&project, &["add", "main-only.txt"]);
    git(&project, &["commit", "-q", "-m", "main work"]);
    git(&project, &["checkout", "-q", "feature"]);

    let vcs = GitVcs::new(&project, &project);
    let info = vcs.info().await;
    assert_eq!(info.branch.current.as_deref(), Some("feature"));
    assert_eq!(info.branch.default.as_deref(), Some("main"));

    let branch = vcs.diff(Mode::Branch, &DiffOptions::default()).await.unwrap();
    assert_eq!(
        summary(&branch),
        vec![
            row("feature.txt", FileStatus::Added, 1, 0),
            row("scratch.txt", FileStatus::Added, 1, 0),
            row("tracked.txt", FileStatus::Modified, 1, 1),
        ]
    );
    // Default context is the whole file.
    assert!(branch[2].patch.contains(" one\n-two\n+changed\n"));
    let committed = vcs.diff(Mode::Committed, &DiffOptions::default()).await.unwrap();
    assert_eq!(summary(&committed), vec![row("feature.txt", FileStatus::Added, 1, 0)]);

    let error = vcs.diff(Mode::Branch, &DiffOptions { base: Some("nope".into()), ..Default::default() }).await.unwrap_err();
    assert_eq!(error.message, "No merge base available for nope");
}

#[tokio::test]
async fn review_base_from_reflog_default_branch_or_error() {
    let (_root, project) = project();
    let vcs = GitVcs::new(&project, &project);
    let base = vcs.base().await.unwrap().unwrap();
    assert_eq!((base.name.as_str(), base.reference.as_str(), base.source), ("main", "refs/heads/main", BaseSource::Default));

    git(&project, &["checkout", "-q", "-b", "topic", "main"]);
    write(project.join("t.txt"), "t\n");
    commit_all(&project, "topic");
    let base = vcs.base().await.unwrap().unwrap();
    assert_eq!((base.name.as_str(), base.reference.as_str(), base.source), ("main", "refs/heads/main", BaseSource::Reflog));

    git(&project, &["checkout", "-q", "-b", "orphaned"]);
    git(&project, &["branch", "-q", "-m", "renamed-branch"]);
    assert_eq!(vcs.base().await.unwrap_err().message, "Choose a review base");

    git(&project, &["checkout", "-q", "--detach"]);
    assert_eq!(vcs.base().await.unwrap_err().message, "Choose a review base");
}

#[tokio::test]
async fn branches_search_limit_and_remote_heads() {
    let root = tmp();
    let fixture = git_remote(&real(root.path()));
    branch(&fixture.source, "feature/docs", "feature\n");
    let checkout = fixture.root.join("checkout");
    git(&fixture.root, &["clone", "-q", &fixture.remote, checkout.to_str().unwrap()]);
    let vcs = GitVcs::new(&checkout, &checkout);
    let all = vcs.branches(&BranchOptions::default()).await;
    assert!(all.contains(&"main".to_string()));
    assert!(all.contains(&"origin/main".to_string()));
    assert!(all.contains(&"origin/feature/docs".to_string()));
    // `refs/remotes/origin/HEAD` lists as `origin/HEAD` and is dropped, never as a bare `origin`.
    assert!(!all.iter().any(|b| b.ends_with("/HEAD") || b == "origin"));
    // Search is a case-insensitive substring match, so it crosses `/`.
    let mut found = vcs.branches(&BranchOptions { search: Some(" MAI ".into()), limit: None }).await;
    found.sort();
    assert_eq!(found, vec!["main", "origin/main"]);
    assert_eq!(vcs.branches(&BranchOptions { search: Some("docs".into()), limit: None }).await, vec!["origin/feature/docs"]);
    assert_eq!(vcs.branches(&BranchOptions { search: None, limit: Some(1) }).await.len(), 1);
    let info = vcs.info().await;
    assert_eq!(info.branch.default.as_deref(), Some("main"));
    assert_eq!(vcs.default_branch().await.unwrap().reference, "origin/main");
}

#[tokio::test]
async fn listings_are_scoped_to_the_location_directory() {
    let (_root, project) = project();
    write(project.join("sub/inner.txt"), "inner\n");
    write(project.join("outer.txt"), "outer\n");
    let vcs = GitVcs::new(project.join("sub"), &project);
    let status = vcs.status().await;
    assert_eq!(status.iter().map(|s| s.file.as_str()).collect::<Vec<_>>(), vec!["sub/inner.txt"]);
    assert_eq!(status[0].additions, 1);
    let diffs = vcs.diff(Mode::Working, &DiffOptions::default()).await.unwrap();
    assert_eq!(summary(&diffs), vec![row("sub/inner.txt", FileStatus::Added, 1, 0)]);
}

// vcs.test.ts "lists remote branches without remote HEAD symrefs"
#[tokio::test]
async fn lists_remote_branches_without_remote_head_symrefs() {
    let (_root, project) = project();
    let directory = project.to_str().unwrap().to_string();
    git(&project, &["tag", "main"]);
    git(&project, &["remote", "add", "origin", &directory]);
    git(&project, &["remote", "add", "fork", &directory]);
    git(&project, &["fetch", "--quiet", "origin"]);
    git(&project, &["fetch", "--quiet", "fork"]);
    git(&project, &["remote", "set-head", "origin", "main"]);
    git(&project, &["remote", "set-head", "fork", "main"]);
    let vcs = GitVcs::new(&project, &project);
    let list = |search: Option<&str>, limit: Option<u64>| {
        let vcs = vcs.clone();
        let search = search.map(str::to_string);
        async move { vcs.branches(&BranchOptions { search, limit }).await }
    };
    assert_eq!(list(None, None).await, vec!["main", "fork/main", "origin/main"]);
    assert_eq!(list(None, Some(2)).await, vec!["main", "fork/main"]);
    assert_eq!(list(Some("origin"), None).await, vec!["origin/main"]);
    assert_eq!(list(Some("MAIN"), Some(2)).await, vec!["main", "fork/main"]);
}

fn folders() -> DiffOptions {
    DiffOptions { untracked: Some(UntrackedMode::Folders), ..Default::default() }
}

// vcs.test.ts "lists an untracked folder as one entry and its entries from inside it when asked for folders"
#[tokio::test]
async fn lists_an_untracked_folder_as_one_entry_when_asked_for_folders() {
    let root = tmp();
    let directory = real(root.path());
    init_repo(&directory);
    write(directory.join("keep.txt"), "one\n");
    write(directory.join(".gitignore"), "artifacts/\nresearch/noisy/a-*\n");
    commit_all(&directory, "initial");
    write(directory.join("keep.txt"), "two\n");
    write(directory.join("research/noisy/z-important.ts"), "keep\n");
    write(directory.join("research/deep/inner.md"), "inner\n");
    for index in 0..505 {
        write(directory.join(format!("research/wide/{index:03}.md")), "wide\n");
        write(directory.join(format!("research/noisy/a-{index}.log")), "ignored\n");
    }
    for index in 0..50 {
        write(directory.join(format!("artifacts/{index}.txt")), "ignored\n");
        write(directory.join(format!("research/{index}.md")), format!("line {index}\n"));
    }
    let review = |scope: &str| {
        let vcs = GitVcs::new(directory.join(scope), &directory);
        async move {
            let diff = vcs.review_diff(Mode::Working, &folders()).await.unwrap();
            let all = vcs.review_diff(Mode::Working, &DiffOptions::default()).await.unwrap();
            let status = vcs.status().await;
            (diff, all, status)
        }
    };
    let files = |rows: &[oc_schema::types::FileDiff]| rows.iter().map(|row| row.file.clone()).collect::<Vec<_>>();

    let (diff, all, status) = review(".").await;
    assert_eq!(files(&diff), vec!["keep.txt", "research/"]);
    assert_eq!((diff[1].status, diff[1].additions, diff[1].deletions), (FileStatus::Added, 0, 0));
    // Callers that do not ask for folders, such as the TUI and plugins, still get every file.
    assert_eq!(all.len(), 1 + 50 + 1 + 505 + 1);
    assert_eq!(status.len(), 1 + 50 + 1 + 505 + 1);

    let (diff, _, _) = review("research").await;
    assert_eq!(diff.len(), 53);
    let seven = diff.iter().find(|row| row.file == "research/7.md").unwrap();
    assert_eq!((seven.status, seven.additions), (FileStatus::Added, 1));
    assert!(seven.patch.contains("+line 7"));
    let listed: Vec<String> = diff.iter().filter(|row| row.file.ends_with('/')).map(|row| row.file.clone()).collect();
    assert_eq!(listed, vec!["research/deep/", "research/noisy/", "research/wide/"]);
    assert!(!diff.iter().any(|row| row.more.is_some()));
    assert_eq!(files(&review("research/deep").await.0), vec!["research/deep/inner.md"]);

    let (wide, _, _) = review("research/wide").await;
    assert_eq!(wide.len(), 501);
    assert_eq!(wide[499].file, "research/wide/499.md");
    assert_eq!(
        serde_json::to_value(&wide[500]).unwrap(),
        serde_json::json!({"file": "research/wide/", "patch": "", "additions": 0, "deletions": 0, "status": "added", "more": true})
    );

    // 505 ignored entries sort before the one that lists.
    let (noisy, _, _) = review("research/noisy").await;
    assert_eq!(
        noisy.iter().map(|row| (row.file.clone(), row.more)).collect::<Vec<_>>(),
        vec![("research/noisy/z-important.ts".to_string(), None)]
    );
}

// vcs.test.ts "stops listing an untracked folder after its scan budget and says more were not checked"
#[tokio::test]
async fn stops_listing_an_untracked_folder_after_its_scan_budget() {
    let root = tmp();
    let directory = real(root.path());
    init_repo(&directory);
    write(directory.join(".gitignore"), "huge/a-*\n");
    commit_all(&directory, "initial");
    for index in 0..5_000 {
        write(directory.join(format!("huge/a-{index:04}.log")), "");
    }
    write(directory.join("huge/z-visible.ts"), "visible\n");
    let vcs = GitVcs::new(directory.join("huge"), &directory);
    let diff = vcs.review_diff(Mode::Working, &folders()).await.unwrap();
    assert_eq!(
        diff.iter().map(|row| (row.file.clone(), row.more)).collect::<Vec<_>>(),
        vec![("huge/".to_string(), Some(true))]
    );
}
