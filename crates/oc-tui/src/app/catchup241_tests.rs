//! The 2.4.1 catch-up of the terminal UI: `/login` and `/auth` (#65265), background jobs in the subagent picker and
//! the prompt footer (#64956), the picker opening on the first busy tab (#64972) and the integration form's text
//! field without an empty description row (#64944).

use super::composer::{BusyCounts, busy_tab, owned_jobs};
use super::tests::{app, render, session};
use super::*;
use crate::api::types::JobInfo;
use crate::keymap::Stroke;
use serde_json::json;

fn joined(lines: &[String]) -> String {
    lines.join("\n")
}

fn job(kind: &str, id: &str, session: &str) -> JobInfo {
    JobInfo { id: id.into(), kind: kind.into(), session_id: session.into(), status: "running".into(), ..Default::default() }
}

fn child(a: &mut App, id: &str, parent: &str) {
    a.store.remember(SessionInfo { id: id.into(), parent_id: Some(parent.into()), title: Some(format!("{id} work")), ..Default::default() });
}

/// Jobs as `experimental.job.updated` delivers them.
fn update(a: &mut App, info: &JobInfo) {
    let fx = a.store.apply(&Event {
        id: format!("evt_{}", info.id),
        kind: "experimental.job.updated".into(),
        data: json!({ "info": serde_json::to_value(info).unwrap() }),
        ..Default::default()
    });
    assert!(fx.is_empty());
}

// ---- #65265: /login and /auth ----------------------------------------------------------------------------------

#[test]
fn login_and_auth_are_aliases_of_connect() {
    let a = app();
    let connect = a.commands().into_iter().find(|c| c.id == "provider.connect").unwrap();
    assert_eq!(connect.slash, Some("connect"));
    assert_eq!(connect.aliases, ["login", "auth"]);
}

// ---- #64956: background jobs -----------------------------------------------------------------------------------

/// `session.test.ts` "ownedJobs follows a subagent job to its child when the child's parent is not loaded".
#[test]
fn owned_jobs_follow_a_subagent_job_to_its_child() {
    let parents = |id: &str| (id == "child").then(|| "session".to_string());
    let codemode = job("codemode", "cm_grandchild", "grandchild");
    let nested = JobInfo { child_session_id: Some("grandchild".into()), ..job("subagent", "sub_nested", "child") };
    let jobs = [&codemode, &nested];
    let ids = |session: &str| owned_jobs(&jobs, session, parents).into_iter().map(|j| j.id.as_str()).collect::<Vec<_>>();
    assert_eq!(ids("child"), ["sub_nested"]);
    assert_eq!(ids("session"), ["sub_nested"]);
    assert_eq!(ids("grandchild"), ["cm_grandchild", "sub_nested"]);
}

/// `composer-tabs.test.tsx` "the picker opens on the first busy tab, else Subagents".
#[test]
fn the_picker_opens_on_the_first_busy_tab() {
    let mut a = app();
    let sid = session(&mut a);
    child(&mut a, "ses_child", &sid);
    a.store.jobs = vec![job("codemode", "job_1", &sid)];
    let script = a.busy_counts(&sid);
    assert_eq!(script, BusyCounts { scripts: 1, subagents: 0, shells: 0 });
    assert_eq!(busy_tab(script), "scripts");

    a.store.jobs.clear();
    let shell = |id: &str, owner: &str| crate::api::types::ShellInfo {
        id: id.into(),
        status: "running".into(),
        command: "sleep 1".into(),
        metadata: Some(json!({ "sessionID": owner })),
    };
    a.store.location_mut("/work/proj").shells = vec![shell("sh_1", &sid), shell("sh_2", "ses_other")];
    let shells = a.busy_counts(&sid);
    assert_eq!(shells, BusyCounts { scripts: 0, subagents: 0, shells: 1 });
    assert_eq!(busy_tab(shells), "shell");
    assert_eq!(busy_tab(BusyCounts::default()), "subagents");

    // `session.child.first` without a tab opens on it.
    a.run_command("session.child.first", None);
    assert_eq!(a.composer.as_ref().map(|c| c.tab.as_str()), Some("shell"));
    a.run_command("session.child.first", None);
    assert!(a.composer.is_none());
    // A running subagent wins over shells.
    a.store.active.insert("ses_child".into(), true);
    a.run_command("session.child.first", None);
    assert_eq!(a.composer.as_ref().map(|c| c.tab.as_str()), Some("subagents"));
    a.composer = None;
    // Scripts first, but without the (opt-in) Scripts tab the picker opens on its first tab.
    a.store.jobs = vec![job("codemode", "job_1", &sid)];
    a.run_command("session.child.first", None);
    assert_eq!(a.composer.as_ref().map(|c| c.tab.as_str()), Some("subagents"));
    a.composer = None;
    a.run_command("session.child.first", Some("scripts".into()));
    assert_eq!(a.composer.as_ref().map(|c| c.tab.as_str()), Some("subagents"));
}

/// `prompt-footer.test.tsx` "prompt footer counts scripts, subagents with jobs, and shells and opens the scripts tab".
#[test]
fn the_footer_counts_scripts_subagents_with_jobs_and_shells() {
    let mut a = app();
    let sid = session(&mut a);
    child(&mut a, "ses_child", &sid);
    child(&mut a, "ses_running", &sid);
    a.store.active.insert("ses_running".into(), true);
    a.store.jobs = vec![job("codemode", "cm_main", &sid), job("codemode", "cm_child", "ses_child")];
    let shell = |id: &str| crate::api::types::ShellInfo {
        id: id.into(),
        status: "running".into(),
        command: "sleep 1".into(),
        metadata: Some(json!({ "sessionID": sid })),
    };
    a.store.location_mut("/work/proj").shells = vec![shell("sh_1"), shell("sh_2")];
    let lines = render(&mut a, 100, 40);
    assert!(lines.iter().any(|l| l.contains("↓ 1 script · 2 subagents · 2 shells")), "{}", joined(&lines));
    // Without the job, the idle child no longer counts.
    a.store.jobs.retain(|j| j.id != "cm_child");
    let lines = render(&mut a, 100, 40);
    assert!(lines.iter().any(|l| l.contains("↓ 1 script · 1 subagent · 2 shells")), "{}", joined(&lines));
}

/// A subagent that owns a running job counts as active in the picker, offers `interrupt`, and moves to the inactive
/// list once the job settles (`experimental.job.updated`).
#[test]
fn a_subagent_with_a_running_job_is_active() {
    let mut a = app();
    let sid = session(&mut a);
    child(&mut a, "ses_child", &sid);
    child(&mut a, "ses_idle", &sid);
    assert!(a.store.job_sync_start(&sid));
    a.store.apply_job_list(&sid, Some(vec![]));
    // A grandchild whose info is not loaded: the subagent job that runs it places it under the child.
    update(&mut a, &JobInfo { child_session_id: Some("ses_grandchild".into()), ..job("subagent", "job_sub", "ses_child") });
    let lines = render(&mut a, 100, 40);
    assert!(lines.iter().any(|l| l.contains("↓ 1 subagent")), "{}", joined(&lines));
    a.subagents_active = true;
    a.run_command("session.child.first", Some("subagents".into()));
    let entries = a.subagent_entries();
    assert_eq!(entries.iter().map(|e| (e.id.as_str(), e.jobs.clone())).collect::<Vec<_>>(), [("ses_child", vec!["job_sub".to_string()])]);
    let lines = render(&mut a, 100, 40);
    assert!(lines.iter().any(|l| l.contains("interrupt")), "{}", joined(&lines));

    update(&mut a, &JobInfo { status: "completed".into(), child_session_id: Some("ses_grandchild".into()), ..job("subagent", "job_sub", "ses_child") });
    assert!(a.subagent_entries().is_empty());
    a.on_key(Stroke::ctrl("a"));
    assert_eq!(a.subagent_entries().iter().map(|e| e.id.as_str()).collect::<Vec<_>>().len(), 2);
}

/// The session route lists its tree's running jobs once its session is loaded, and a reconnect lists them again.
#[test]
fn the_session_route_syncs_its_tree_jobs() {
    let mut a = app();
    let sid = session(&mut a);
    a.route = Route::Home;
    a.navigate(Route::Session(sid.clone()));
    // The list is in flight: a second sync joins it.
    assert!(!a.store.job_sync_start(&sid));
    a.store.apply_job_list(&sid, Some(vec![job("codemode", "cm_1", &sid)]));
    assert_eq!(a.store.jobs_for(&sid).len(), 1);
    let fx = a.store.apply(&Event { id: "evt_connected".into(), kind: "server.connected".into(), data: json!({}), ..Default::default() });
    assert!(fx.contains(&crate::store::Effect::JobSync(sid.clone())));
}

// ---- #64983: the Worktrees dialog keeps its footer at the bottom --------------------------------------------------

#[test]
fn the_worktrees_dialog_fills_its_minimum_height_with_the_footer_at_the_bottom() {
    let mut a = app();
    session(&mut a);
    a.store.location_mut("/work/proj").info =
        Some(LocationInfo { directory: "/work/proj".into(), project: ProjectRef { id: "prj_1".into(), canonical: "/work/proj".into(), ..Default::default() } });
    a.run_command("session.move", None);
    if let Some(sel) = a.dialogs.last_mut().and_then(|d| d.sel()) {
        sel.empty = Some("No worktrees available".into());
    }
    // 40 rows: `max(8, min(16, 40 - 10 - 2))` = 16 rows from the title to the bottom padding (checked against the
    // 2.4.1 build).
    let lines = render(&mut a, 120, 40);
    let title = lines.iter().position(|l| l.contains("Worktrees") && l.ends_with("esc")).unwrap_or_else(|| panic!("{}", joined(&lines)));
    let footer = lines.iter().position(|l| l.contains("new ctrl+a")).unwrap();
    assert_eq!(footer - title, 14, "{}", joined(&lines));
    assert_eq!(lines[title + 4].trim(), "No worktrees available");
    // 14 rows: `14 - 3 - 2` = 9; 12 rows: the minimum 8, which the list's own rows fill.
    for (rows, gap) in [(14, 7), (12, 6)] {
        let lines = render(&mut a, 120, rows);
        let title = lines.iter().position(|l| l.contains("Worktrees") && l.ends_with("esc")).unwrap_or_else(|| panic!("{}", joined(&lines)));
        let footer = lines.iter().position(|l| l.contains("new ctrl+a")).unwrap();
        assert_eq!(footer - title, gap, "{}", joined(&lines));
    }
}
