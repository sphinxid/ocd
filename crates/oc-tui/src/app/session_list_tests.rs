//! The session list dialog: project scope, `ctrl+a`, server search results and the footer.

use super::tests::{app, render};
use super::*;
use crate::keymap::Stroke;

fn row<'a>(lines: &'a [String], needle: &str) -> &'a str {
    lines.iter().find(|l| l.contains(needle)).map(|s| s.as_str()).unwrap_or_else(|| panic!("no row with {needle:?}:\n{}", lines.join("\n")))
}

fn add(a: &mut App, id: &str, title: &str, project: &str, dir: &str, updated: i64) {
    a.store.remember(SessionInfo {
        id: id.into(),
        title: Some(title.into()),
        project_id: project.into(),
        location: LocationRef { directory: dir.into(), workspace_id: None },
        time: SessionTime { created: updated, updated, ..Default::default() },
        ..Default::default()
    });
}

fn setup() -> App {
    let mut a = app();
    a.store.location_mut("/work/proj").info = Some(LocationInfo {
        directory: "/work/proj".into(),
        project: ProjectRef { id: "prj_1".into(), directory: "/work/proj".into(), canonical: "/work/proj".into() },
    });
    add(&mut a, "ses_a", "Here", "prj_1", "/work/proj", 3000);
    add(&mut a, "ses_b", "Other project", "prj_2", "/work/other", 2000);
    a
}

#[test]
fn scoped_to_the_project_directory_until_ctrl_a() {
    let mut a = setup();
    a.open_session_list();
    // Upstream shows "Loading sessions…" until the first search lands; then (no server here) the
    // local list.
    assert!(render(&mut a, 100, 30).join("\n").contains("Loading sessions…"));
    a.session_search.loading = false;
    a.refresh_session_list();
    let lines = render(&mut a, 100, 30);
    let joined = lines.join("\n");
    assert!(row(&lines, "Sessions for proj").contains("esc"), "{joined}");
    assert!(joined.contains("Here"), "{joined}");
    assert!(!joined.contains("Other project"), "{joined}");
    assert!(row(&lines, "delete ctrl+d").trim_end().ends_with("all projects ctrl+a"), "{joined}");
    a.on_key(Stroke::ctrl("a"));
    assert_eq!(a.keyed.get("session-list"), Some(serde_json::json!({ "allProjects": true })));
    a.session_search.loading = false;
    a.refresh_session_list();
    let lines = render(&mut a, 100, 30);
    let joined = lines.join("\n");
    assert!(!joined.contains("Sessions for proj"), "{joined}");
    assert!(joined.contains("Other project"), "{joined}");
    assert!(row(&lines, "delete ctrl+d").trim_end().ends_with("current directory ctrl+a"), "{joined}");
}

#[test]
fn search_results_replace_the_local_list() {
    let mut a = setup();
    a.open_session_list();
    // The failed search (no server) falls back to the local list.
    a.session_search.loading = false;
    a.session_search.latest = Some(session_list::SearchResult { query: String::new(), all: false, sessions: vec![], error: true });
    a.refresh_session_list();
    let joined = render(&mut a, 100, 30).join("\n");
    assert!(joined.contains("Here"), "{joined}");
    // A result for the current query and scope wins.
    let found = SessionInfo { id: "ses_c".into(), title: Some("From the server".into()), project_id: "prj_1".into(), ..Default::default() };
    a.session_search.latest = Some(session_list::SearchResult { query: String::new(), all: false, sessions: vec![found], error: false });
    a.refresh_session_list();
    let joined = render(&mut a, 100, 30).join("\n");
    assert!(joined.contains("From the server") && !joined.contains("Here"), "{joined}");
    // An empty failed search shows the error in place of the list.
    a.session_search.latest = Some(session_list::SearchResult { query: String::new(), all: false, sessions: vec![], error: true });
    a.store.sessions.clear();
    a.refresh_session_list();
    let joined = render(&mut a, 100, 30).join("\n");
    assert!(joined.contains("Could not load sessions."), "{joined}");
}

#[test]
fn typing_filters_locally_while_the_search_waits() {
    let mut a = setup();
    add(&mut a, "ses_d", "Another here", "prj_1", "/work/proj", 2500);
    a.open_session_list();
    a.session_search.loading = false;
    a.session_search.latest = None;
    for c in "anoth".chars() {
        a.on_key(Stroke::key(&c.to_string()));
    }
    let joined = render(&mut a, 100, 30).join("\n");
    assert!(joined.contains("Another here") && !joined.contains("│  Here"), "{joined}");
    assert!(!joined.lines().any(|l| l.trim() == "Here"), "{joined}");
}
