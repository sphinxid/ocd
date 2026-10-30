//! Project archiving (`core/test/project.test.ts`, "Project archiving").

mod common;

use common::*;
use serde_json::{Value, json};

/// Every event published so far on `events`.
fn drain(
    events: &mut tokio::sync::broadcast::Receiver<oc_core::bus::Event>,
) -> Vec<oc_core::bus::Event> {
    let mut out = vec![];
    while let Ok(e) = events.try_recv() {
        out.push(e);
    }
    out
}

fn insert_project(
    h: &Harness,
    id: &str,
    worktree: &str,
    updated: i64,
    active: i64,
    archived: Option<i64>,
) {
    h.app
        .bus
        .db()
        .with(|c| {
            c.execute(
                "INSERT INTO project (id, worktree, sandboxes, time_created, time_updated, time_active, time_archived) VALUES (?, ?, '[]', 1, ?, ?, ?)",
                rusqlite::params![id, worktree, updated, active, archived],
            )
        })
        .unwrap();
}

/// Backdates every Project past the sweep's recent-activity window.
fn idle(h: &Harness) {
    h.app
        .bus
        .db()
        .with(|c| c.execute("UPDATE project SET time_updated = 1, time_active = 1", []))
        .unwrap();
}

fn project_ids(list: &[Value]) -> Vec<String> {
    list.iter()
        .map(|p| p["id"].as_str().unwrap().to_string())
        .collect()
}

#[tokio::test]
async fn lists_archived_projects_only_when_requested() {
    let h = harness();
    insert_project(&h, "active", "/active", 1, 2, None);
    insert_project(&h, "archived", "/archived", 1, 1, Some(5));

    assert_eq!(
        project_ids(&oc_core::project::list(&h.app.bus, false).unwrap()),
        vec!["active"]
    );
    let all = oc_core::project::list(&h.app.bus, true).unwrap();
    assert_eq!(project_ids(&all), vec!["active", "archived"]);
    assert_eq!(all[1]["time"]["archived"], json!(5));
    assert!(all[0]["time"].get("archived").is_none());

    // Since 2.2 the project routes take no location: listing boots nothing, so the server's own directory is
    // not resolved (or unarchived) as a side effect.
    let listed = |v: Value| project_ids(v.as_array().unwrap());
    assert_eq!(listed(h.get("/api/project").await.json()), vec!["active".to_string()]);
    assert_eq!(listed(h.get("/api/project?archived=false").await.json()), vec!["active".to_string()]);
    assert_eq!(
        listed(h.get("/api/project?archived=true").await.json()),
        vec!["active".to_string(), "archived".into()]
    );
    let r = h.get("/api/project?archived=x").await;
    assert_eq!(r.status, 400);
}

#[tokio::test]
async fn archives_projects_whose_directories_are_gone_and_unarchives_them_on_resolve() {
    let h = harness();
    let tmp = tempfile::tempdir().unwrap();
    let base = std::fs::canonicalize(tmp.path()).unwrap();
    let (kept, removed) = (base.join("kept"), base.join("removed"));
    std::fs::create_dir(&kept).unwrap();
    std::fs::create_dir(&removed).unwrap();
    let bus = &h.app.bus;
    let kept_project = oc_core::project::resolve_and_persist(bus, &kept)
        .await
        .unwrap();
    let removed_project = oc_core::project::resolve_and_persist(bus, &removed)
        .await
        .unwrap();
    let r = h
        .req(
            "PATCH",
            &format!("/api/project/{}", removed_project.id),
            Some(json!({"name": "Removed"})),
        )
        .await;
    assert_eq!(r.status, 200);
    idle(&h);
    let find = |list: Vec<Value>, id: &str| list.into_iter().find(|p| p["id"] == id);
    let updated = find(
        oc_core::project::list(bus, false).unwrap(),
        &removed_project.id,
    )
    .unwrap()["time"]["updated"]
        .clone();
    let mut events = bus.subscribe();

    std::fs::remove_dir_all(&removed).unwrap();
    oc_core::project::sweep(bus).unwrap();
    oc_core::project::sweep(bus).unwrap();

    // (The PATCH above booted the server's own directory, whose project is listed too.)
    let cwd_project = oc_util::hash::fast(format!("directory:{}", h.dir()));
    let listed: Vec<String> = project_ids(&oc_core::project::list(bus, false).unwrap())
        .into_iter()
        .filter(|id| *id != cwd_project)
        .collect();
    assert_eq!(listed, vec![kept_project.id.clone()]);
    let archived = find(
        oc_core::project::list(bus, true).unwrap(),
        &removed_project.id,
    )
    .unwrap();
    assert!(archived["time"]["archived"].is_number());
    assert_eq!(archived["time"]["updated"], updated);
    let updates: Vec<Value> = drain(&mut events)
        .iter()
        .filter(|e| e.kind == "project.updated")
        .map(|e| e.data.clone())
        .collect();
    assert_eq!(updates, vec![archived.clone()]);

    std::fs::create_dir(&removed).unwrap();
    oc_core::project::resolve_and_persist(bus, &removed)
        .await
        .unwrap();
    oc_core::project::resolve_and_persist(bus, &removed)
        .await
        .unwrap();

    let restored = find(
        oc_core::project::list(bus, false).unwrap(),
        &removed_project.id,
    )
    .unwrap();
    assert_eq!(restored["name"], "Removed");
    assert_eq!(restored["time"]["updated"], updated);
    assert!(restored["time"].get("archived").is_none());
    let updates: Vec<Value> = drain(&mut events)
        .iter()
        .filter(|e| e.kind == "project.updated")
        .map(|e| e.data.clone())
        .collect();
    assert_eq!(updates, vec![restored]);
}

#[tokio::test]
async fn moves_the_canonical_directory_to_a_surviving_clone_instead_of_archiving() {
    if !git_available() {
        return;
    }
    let h = harness();
    let tmp = tempfile::tempdir().unwrap();
    let base = std::fs::canonicalize(tmp.path()).unwrap();
    let (main, clone, linked) = (base.join("repo"), base.join("clone"), base.join("linked"));
    std::fs::create_dir(&main).unwrap();
    git(&main, &["init", "-q"]);
    git(
        &main,
        &["remote", "add", "origin", "git@github.com:owner/repo.git"],
    );
    git(&main, &["commit", "-q", "--allow-empty", "-m", "init"]);
    git(
        &base,
        &[
            "clone",
            "-q",
            "--no-hardlinks",
            main.to_str().unwrap(),
            clone.to_str().unwrap(),
        ],
    );
    git(
        &clone,
        &[
            "remote",
            "set-url",
            "origin",
            "git@github.com:owner/repo.git",
        ],
    );
    git(
        &main,
        &[
            "worktree",
            "add",
            "-q",
            linked.to_str().unwrap(),
            "-b",
            "linked",
        ],
    );
    let bus = &h.app.bus;
    let initial = oc_core::project::resolve_and_persist(bus, &main)
        .await
        .unwrap();
    oc_core::project::resolve_and_persist(bus, &linked)
        .await
        .unwrap();
    oc_core::project::resolve_and_persist(bus, &clone)
        .await
        .unwrap();
    idle(&h);

    std::fs::remove_dir_all(&main).unwrap();
    oc_core::project::sweep(bus).unwrap();

    let info = oc_core::project::get(bus, &initial.id).unwrap().unwrap();
    assert_eq!(info["canonical"], json!(clone.display().to_string()));
    assert!(info["time"].get("archived").is_none());
}

#[tokio::test]
async fn sweep_skips_recent_projects_and_never_archives_the_global_project() {
    let h = harness();
    let now = oc_schema::id::now_ms() as i64;
    let old = now - 2 * 60 * 60 * 1000;
    insert_project(&h, "updated", "/opencode-missing-updated", now, old, None);
    insert_project(&h, "active", "/opencode-missing-active", old, now, None);
    insert_project(&h, "idle", "/opencode-missing-idle", old, old, None);
    insert_project(&h, "global", "/opencode-missing-global", 1, 1, None);

    oc_core::project::sweep(&h.app.bus).unwrap();

    let mut listed = project_ids(&oc_core::project::list(&h.app.bus, false).unwrap());
    listed.sort();
    assert_eq!(listed, vec!["active", "global", "updated"]);
}
