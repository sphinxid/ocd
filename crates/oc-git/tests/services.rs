mod common;

use std::path::{Path, PathBuf};
use std::sync::Arc;

use common::*;
use oc_git::repository::{self, cache_path};
use oc_git::snapshot::store_directory;
use oc_git::worktree::{GitWorktreeStrategy, ListEntryType, allocate_directory};
use oc_git::{EnsureInput, Git, Kv, MemoryKv, Refresh, RepositoryCache, SnapshotLocation, Snapshots, Status, TreeId};

fn snapshots(data: &Path, project: &Path, directory: &Path) -> Snapshots {
    Snapshots::new(
        Git::new(),
        SnapshotLocation {
            data: data.to_path_buf(),
            project_id: "prj_test".to_string(),
            project_directory: project.to_path_buf(),
            directory: directory.to_path_buf(),
            vcs: Some("git".to_string()),
        },
    )
}

fn strings(items: &[&str]) -> Vec<String> {
    items.iter().map(|s| s.to_string()).collect()
}

#[tokio::test]
async fn captures_and_restores_location_scoped_changes() {
    let root = tmp();
    let base = real(root.path());
    let project = base.join("project");
    let location = project.join("scope");
    write(location.join("tracked.txt"), "one\n");
    write(project.join("outside.txt"), "outside\n");
    init_git(&project, false);
    let snapshot = snapshots(&base, &project, &location);
    let before = snapshot.capture().await.expect("capture");
    write(location.join("tracked.txt"), "two\n");
    write(location.join("added.txt"), "added\n");
    write(project.join("outside.txt"), "changed outside\n");
    let after = snapshot.capture().await.expect("capture");
    assert_eq!(snapshot.files(&before, &after).await.unwrap(), strings(&["scope/added.txt", "scope/tracked.txt"]));
    let diffs = snapshot.diff(&before, &after, None, None).await.unwrap();
    assert_eq!(diffs.len(), 2);
    assert!(diffs[1].patch.contains("-one\n+two\n"));
    snapshot.restore(&[("scope/tracked.txt".to_string(), before.clone())]).await.unwrap();
    assert_eq!(read(location.join("tracked.txt")), "one\n");
    assert_eq!(read(location.join("added.txt")), "added\n");
    assert_eq!(read(project.join("outside.txt")), "changed outside\n");
    // Store layout: <data>/snapshot/<projectID>/<sha1(realpath(worktree))>
    let store = base.join("snapshot").join("prj_test").join(oc_util::hash::fast(project.to_string_lossy().as_bytes()));
    assert_eq!(store_directory(&base, "prj_test", &project), store);
    assert!(store.join("HEAD").exists());
    assert!(store.join("opencode.gitconfig").exists());
}

#[tokio::test]
async fn hides_paths_the_source_ignores() {
    let root = tmp();
    let base = real(root.path());
    let project = base.join("project");
    write(project.join("tracked.txt"), "one\n");
    write(project.join(".gitignore"), "*.log\n");
    init_git(&project, true);
    let snapshot = snapshots(&base, &project, &project);
    let before = snapshot.capture().await.unwrap();
    write(project.join("debug.log"), "noise\n");
    write(project.join("tracked.txt"), "two\n");
    let after = snapshot.capture().await.unwrap();
    assert_eq!(snapshot.files(&before, &after).await.unwrap(), strings(&["tracked.txt"]));
    let diffs = snapshot.diff(&before, &after, Some(0), None).await.unwrap();
    assert_eq!(diffs.iter().map(|d| d.file.as_str()).collect::<Vec<_>>(), vec!["tracked.txt"]);
    assert!(snapshot.diff(&before, &after, None, Some(vec![])).await.unwrap().is_empty());
}

#[tokio::test]
async fn recovers_from_a_corrupt_index_and_ignores_a_stale_index_lock() {
    let root = tmp();
    let base = real(root.path());
    let project = base.join("project");
    write(project.join("tracked.txt"), "one\n");
    init_git(&project, true);
    let snapshot = snapshots(&base, &project, &project);
    let before = snapshot.capture().await.unwrap();
    let storage = store_directory(&base, "prj_test", &project);
    std::fs::write(storage.join("index"), vec![0u8; 512]).unwrap();
    std::fs::write(storage.join("index.lock"), "").unwrap();
    write(project.join("tracked.txt"), "two\n");
    let after = snapshot.capture().await.expect("capture after corruption");
    assert_eq!(snapshot.files(&before, &after).await.unwrap(), strings(&["tracked.txt"]));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn captures_concurrently_from_independent_services() {
    let root = tmp();
    let base = real(root.path());
    let project = base.join("project");
    for index in 0..200 {
        write(project.join(format!("f{index}.txt")), format!("{index}\n"));
    }
    init_git(&project, true);
    let writer = |id: usize| {
        let base = base.clone();
        let project = project.clone();
        async move {
            // Each service owns its own Git, so their in-process locks do not coordinate.
            let snapshot = snapshots(&base, &project, &project);
            let mut out = Vec::new();
            for index in 0..8 {
                write(project.join(format!("writer-{id}.txt")), format!("{index}\n"));
                out.push(snapshot.capture().await);
            }
            out
        }
    };
    let results = futures::future::join_all((0..3).map(|id| tokio::spawn(writer(id)))).await;
    assert!(results.into_iter().flat_map(|r| r.unwrap()).all(|tree| tree.is_some()));
}

#[tokio::test]
async fn captures_a_location_whose_name_starts_with_two_dots() {
    let root = tmp();
    let base = real(root.path());
    let project = base.join("project");
    let location = project.join("..scope");
    write(location.join("tracked.txt"), "one\n");
    init_git(&project, false);
    let snapshot = snapshots(&base, &project, &location);
    let before = snapshot.capture().await.unwrap();
    write(location.join("tracked.txt"), "two\n");
    let after = snapshot.capture().await.unwrap();
    assert_eq!(snapshot.files(&before, &after).await.unwrap(), strings(&["..scope/tracked.txt"]));
}

#[tokio::test]
async fn restores_many_files_from_several_trees_and_removes_absent_paths() {
    let root = tmp();
    let base = real(root.path());
    let project = base.join("project");
    write(project.join("a.txt"), "a1\n");
    write(project.join("b[1].txt"), "b1\n");
    init_git(&project, true);
    let snapshot = snapshots(&base, &project, &project);
    let first = snapshot.capture().await.unwrap();
    write(project.join("a.txt"), "a2\n");
    write(project.join("c.txt"), "c2\n");
    let second = snapshot.capture().await.unwrap();
    write(project.join("a.txt"), "a3\n");
    write(project.join("b[1].txt"), "b3\n");
    write(project.join("c.txt"), "c3\n");
    write(project.join("d.txt"), "d3\n");
    snapshot
        .restore(&[
            ("a.txt".to_string(), second.clone()),
            ("b[1].txt".to_string(), first.clone()),
            ("c.txt".to_string(), first.clone()),
            ("d.txt".to_string(), second.clone()),
        ])
        .await
        .unwrap();
    assert_eq!(read(project.join("a.txt")), "a2\n");
    assert_eq!(read(project.join("b[1].txt")), "b1\n");
    assert!(!project.join("c.txt").exists());
    assert!(!project.join("d.txt").exists());
}

#[tokio::test]
async fn restores_and_diffs_a_selection_too_long_for_one_command_line() {
    let root = tmp();
    let base = real(root.path());
    let project = base.join("project");
    let names: Vec<String> = (0..300).map(|i| format!("deep/{}-{i}.txt", "n".repeat(90))).collect();
    for name in &names {
        write(project.join(name), "one\n");
    }
    init_git(&project, true);
    let snapshot = snapshots(&base, &project, &project);
    let first = snapshot.capture().await.unwrap();
    for name in &names {
        write(project.join(name), "two\n");
    }
    let second = snapshot.capture().await.unwrap();
    let diffs = snapshot.diff(&first, &second, None, Some(names.clone())).await.unwrap();
    let mut files: Vec<String> = diffs.iter().map(|d| d.file.clone()).collect();
    files.sort();
    let mut sorted = names.clone();
    sorted.sort();
    assert_eq!(files, sorted);
    assert!(diffs.iter().all(|d| d.additions == 1 && d.deletions == 1 && d.patch.contains("+two")));
    let plan: Vec<(String, TreeId)> = names.iter().map(|n| (n.clone(), first.clone())).collect();
    snapshot.restore(&plan).await.unwrap();
    assert!(names.iter().all(|n| read(project.join(n)) == "one\n"));
}

#[tokio::test]
async fn restores_the_other_files_when_removing_one_path_fails() {
    let root = tmp();
    let base = real(root.path());
    let project = base.join("project");
    write(project.join("c.txt"), "old\n");
    init_git(&project, true);
    let snapshot = snapshots(&base, &project, &project);
    let first = snapshot.capture().await.unwrap();
    write(project.join("c.txt"), "changed\n");
    write(project.join("a"), "file\n");
    let result = snapshot.restore(&[("a/b".to_string(), first.clone()), ("c.txt".to_string(), first)]).await;
    assert!(result.is_err());
    assert_eq!(read(project.join("c.txt")), "old\n");
}

#[tokio::test]
async fn rejects_paths_escaping_the_project() {
    let root = tmp();
    let base = real(root.path());
    let project = base.join("project");
    write(project.join("a.txt"), "a\n");
    init_git(&project, true);
    let snapshot = snapshots(&base, &project, &project);
    let tree = snapshot.capture().await.unwrap();
    let error = snapshot.restore(&[("../x".to_string(), tree)]).await.unwrap_err();
    assert_eq!(error.message, "Path escapes the project: ../x");
}

#[tokio::test]
async fn applies_availability_and_rejects_non_git_locations() {
    let root = tmp();
    let base = real(root.path());
    let project = base.join("project");
    write(project.join("tracked.txt"), "one\n");
    init_git(&project, false);
    let snapshot = snapshots(&base, &project, &project);
    snapshot.configure(false);
    assert!(snapshot.capture().await.is_none());
    let error = snapshot.restore(&[]).await.unwrap_err();
    assert_eq!(error.message, "Snapshots are disabled");
    snapshot.configure(true);
    assert!(snapshot.capture().await.is_some());

    let outside = base.join("plain");
    std::fs::create_dir_all(&outside).unwrap();
    if Git::new().discover(&outside).await.is_none() {
        let plain = snapshots(&base, &outside, &outside);
        assert!(plain.capture().await.is_none());
        let error = plain.files(&TreeId::from("a"), &TreeId::from("b")).await.unwrap_err();
        assert_eq!(error.message, "Project is not a Git repository");
        assert_eq!(error.operation, oc_git::SnapshotOperation::Files);
    }
}

#[tokio::test]
async fn isolates_snapshot_stores_by_canonical_worktree() {
    let root = tmp();
    let base = real(root.path());
    let project = base.join("project");
    let linked = base.join("linked");
    write(project.join("tracked.txt"), "main\n");
    init_git(&project, true);
    git(&project, &["-c", "core.fsmonitor=false", "worktree", "add", "--detach", linked.to_str().unwrap(), "HEAD"]);
    assert!(snapshots(&base, &project, &project).capture().await.is_some());
    assert!(snapshots(&base, &linked, &linked).capture().await.is_some());
    assert!(store_directory(&base, "prj_test", &project).exists());
    assert!(store_directory(&base, "prj_test", &linked).exists());
}

// ------------------------------------------------------------------ worktree strategy

#[tokio::test]
async fn git_worktree_strategy_creates_lists_and_removes() {
    let root = tmp();
    let base = real(root.path());
    let project = base.join("project");
    init_repo(&project);
    let parent = oc_git::worktree::default_directory(&base, "abcdef123456");
    let directory = allocate_directory(&parent, None).unwrap();
    assert!(directory.starts_with(base.join("worktree").join("abcdef")));
    let strategy = GitWorktreeStrategy::new(Git::new());
    let created = strategy.create(&directory, &project, None).await.unwrap();
    assert_eq!(created, real(&directory));
    let entries = strategy.list(&project).await.unwrap();
    assert_eq!(entries[0].kind, ListEntryType::Root);
    assert_eq!(entries[0].directory, project);
    assert!(entries.iter().any(|e| e.kind == ListEntryType::Worktree && e.directory == created));
    strategy.remove(&created, false).await.unwrap();
    assert!(!strategy.list(&project).await.unwrap().iter().any(|e| e.directory == created));
}

// ------------------------------------------------------------------ repository cache

fn reference_for(remote: &str) -> oc_git::Reference {
    let mut reference = repository::parse_remote("owner/repo").unwrap();
    reference.remote = remote.to_string();
    reference
}

fn cache(base: &Path, kv: Arc<dyn Kv>) -> RepositoryCache {
    RepositoryCache::new(Git::new(), base.join("repos"), base.join("locks"), kv)
}

fn ensure(remote: &str, refresh: Refresh, branch: Option<&str>) -> EnsureInput {
    EnsureInput { reference: reference_for(remote), refresh, branch: branch.map(str::to_string) }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn repository_cache_throttles_daily_refreshes_and_serializes_competitors() {
    let root = tmp();
    let base = real(root.path());
    let fixture = git_remote(&base);
    let kv: Arc<dyn Kv> = Arc::new(MemoryKv::default());
    let initial = cache(&base, kv.clone()).ensure(ensure(&fixture.remote, Refresh::Daily, None)).await.unwrap();
    assert_eq!(initial.status, Status::Cloned);
    assert_eq!(initial.repository, "owner/repo");
    assert_eq!(initial.host, "github.com");
    assert_eq!(PathBuf::from(&initial.local_path), cache_path(&base.join("repos"), &reference_for(&fixture.remote), None));
    assert_eq!(initial.branch.as_deref(), Some("main"));
    let key = format!("repository-cache:{}", initial.local_path);
    assert!(kv.get(&key).unwrap().unwrap()["attemptedAt"].as_u64().is_some());
    commit_readme(&fixture.source, "two\n", "advance main");

    let second = cache(&base, kv.clone()).ensure(ensure(&fixture.remote, Refresh::Daily, None)).await.unwrap();
    assert_eq!(second.status, Status::Cached);
    assert_eq!(read(Path::new(&initial.local_path).join("README.md")), "one\n");
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_millis() as u64;
    kv.set(&key, &serde_json::json!({ "attemptedAt": now - 24 * 60 * 60 * 1000 })).unwrap();

    let a = tokio::spawn({
        let (base, kv, remote) = (base.clone(), kv.clone(), fixture.remote.clone());
        async move { cache(&base, kv).ensure(ensure(&remote, Refresh::Daily, None)).await }
    });
    let b = tokio::spawn({
        let (base, kv, remote) = (base.clone(), kv.clone(), fixture.remote.clone());
        async move { cache(&base, kv).ensure(ensure(&remote, Refresh::Daily, None)).await }
    });
    let mut statuses = vec![a.await.unwrap().unwrap().status, b.await.unwrap().unwrap().status];
    statuses.sort_by_key(|s| format!("{s:?}"));
    assert_eq!(statuses, vec![Status::Cached, Status::Refreshed]);
    assert_eq!(read(Path::new(&initial.local_path).join("README.md")), "two\n");

    // A missing checkout must be recreated even when the persisted timestamp is recent.
    std::fs::remove_dir_all(&initial.local_path).unwrap();
    let recreated = cache(&base, kv.clone()).ensure(ensure(&fixture.remote, Refresh::Daily, None)).await.unwrap();
    assert_eq!(recreated.status, Status::Cloned);
    assert_eq!(read(Path::new(&initial.local_path).join("README.md")), "two\n");
}

#[tokio::test]
async fn repository_cache_honors_legacy_records_and_forced_refreshes() {
    let root = tmp();
    let base = real(root.path());
    let fixture = git_remote(&base);
    let kv: Arc<dyn Kv> = Arc::new(MemoryKv::default());
    let service = cache(&base, kv.clone());
    let initial = service.ensure(ensure(&fixture.remote, Refresh::Daily, None)).await.unwrap();
    commit_readme(&fixture.source, "two\n", "advance main");
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_millis() as u64;
    kv.set(&format!("repository-cache:{}", initial.local_path), &serde_json::json!({ "attemptedAt": now, "refreshedAt": 0 }))
        .unwrap();
    assert_eq!(service.ensure(ensure(&fixture.remote, Refresh::Daily, None)).await.unwrap().status, Status::Cached);
    assert_eq!(read(Path::new(&initial.local_path).join("README.md")), "one\n");
    assert_eq!(service.ensure(ensure(&fixture.remote, Refresh::Always, None)).await.unwrap().status, Status::Refreshed);
    assert_eq!(read(Path::new(&initial.local_path).join("README.md")), "two\n");
    assert_eq!(service.ensure(ensure(&fixture.remote, Refresh::Never, None)).await.unwrap().status, Status::Cached);
}

#[tokio::test]
async fn repository_cache_tracks_branches_in_their_own_checkout() {
    let root = tmp();
    let base = real(root.path());
    let fixture = git_remote(&base);
    branch(&fixture.source, "feature/docs", "feature\n");
    let service = cache(&base, Arc::new(MemoryKv::default()));
    let result = service.ensure(ensure(&fixture.remote, Refresh::Never, Some("feature/docs"))).await.unwrap();
    assert_eq!(result.status, Status::Cloned);
    assert!(result.local_path.ends_with("repo@feature%2Fdocs"));
    assert_eq!(result.branch.as_deref(), Some("feature/docs"));
    assert_eq!(read(Path::new(&result.local_path).join("README.md")), "feature\n");
    let refreshed = service.ensure(ensure(&fixture.remote, Refresh::Always, Some("feature/docs"))).await.unwrap();
    assert_eq!(refreshed.status, Status::Refreshed);
    assert_eq!(refreshed.branch.as_deref(), Some("feature/docs"));
    let error = service.ensure(ensure(&fixture.remote, Refresh::Never, Some("-bad"))).await.unwrap_err();
    assert_eq!(error.tag(), "RepositoryCacheInvalidBranchError");
}

#[tokio::test]
async fn repository_cache_reports_clone_failures_after_recording_the_attempt() {
    let root = tmp();
    let base = real(root.path());
    let kv: Arc<dyn Kv> = Arc::new(MemoryKv::default());
    let missing = url::Url::from_file_path(base.join("missing.git")).unwrap().to_string();
    let error = cache(&base, kv.clone()).ensure(ensure(&missing, Refresh::Daily, None)).await.unwrap_err();
    assert_eq!(error.tag(), "RepositoryCacheCloneFailedError");
    let local = cache_path(&base.join("repos"), &reference_for(&missing), None);
    assert!(kv.get(&format!("repository-cache:{}", local.display())).unwrap().is_some());
}
