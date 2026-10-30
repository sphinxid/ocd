//! opencode 2.2 filesystem, VCS and terminal endpoints: `vcs.diff ?untracked=folders`,
//! `POST`/`DELETE /api/experimental/fs/watch`, the persistent terminal cwd check, project icons
//! and session turn diffs. Expected bodies were checked against the 2.2.2 binary.

mod common;

use common::*;
use serde_json::{Value, json};

async fn next_event(
    events: &mut tokio::sync::broadcast::Receiver<oc_core::bus::Event>,
    kind: &str,
) -> oc_core::bus::Event {
    tokio::time::timeout(std::time::Duration::from_secs(10), async {
        loop {
            let e = events.recv().await.unwrap();
            if e.kind == kind {
                return e;
            }
        }
    })
    .await
    .unwrap_or_else(|_| panic!("no {kind}"))
}

#[tokio::test]
async fn vcs_diff_lists_untracked_folders_when_asked() {
    if !git_available() {
        return;
    }
    let h = harness();
    git(&h.cwd, &["init", "-q"]);
    std::fs::write(h.cwd.join("keep.txt"), "one\n").unwrap();
    git(&h.cwd, &["add", "."]);
    git(&h.cwd, &["commit", "-qm", "init"]);
    std::fs::create_dir_all(h.cwd.join("research/deep")).unwrap();
    std::fs::write(h.cwd.join("research/a.md"), "a\n").unwrap();
    std::fs::write(h.cwd.join("research/deep/b.md"), "b\n").unwrap();
    let files =
        |r: &Res| r.json()["data"].as_array().unwrap().iter().map(|row| row["file"].clone()).collect::<Vec<_>>();
    let r = h.get("/api/vcs/diff?mode=working&untracked=folders").await;
    assert_eq!(r.status, 200, "{}", String::from_utf8_lossy(&r.body));
    assert_eq!(files(&r), vec![json!("research/")]);
    let row = &r.json()["data"][0];
    assert_eq!((row["status"].clone(), row["additions"].clone(), row.get("more")), (json!("added"), json!(0), None));
    let r = h.get("/api/vcs/diff?mode=working&untracked=files").await;
    assert_eq!(files(&r), vec![json!("research/a.md"), json!("research/deep/b.md")]);
    let r = h.get("/api/vcs/diff?mode=working").await;
    assert_eq!(files(&r), vec![json!("research/a.md"), json!("research/deep/b.md")]);
    // Inside the untracked folder, its entries list one level deep.
    let research = h.cwd.join("research").display().to_string();
    let r = h.req_in(Some(&research), "GET", "/api/vcs/diff?mode=working&untracked=folders", None).await;
    assert_eq!(files(&r), vec![json!("research/a.md"), json!("research/deep/")]);
    let r = h.get("/api/vcs/diff?mode=working&untracked=dirs").await;
    assert_eq!(r.status, 400);
    assert_eq!(r.json()["_tag"], "InvalidRequestError");
}

#[tokio::test]
async fn fs_watch_publishes_changes_of_one_file() {
    let h = harness();
    std::fs::write(h.cwd.join("a.txt"), "one\n").unwrap();
    let r = h.post("/api/experimental/fs/watch", json!({ "path": "missing.txt" })).await;
    assert_eq!(r.status, 404);
    assert_eq!(
        r.json(),
        json!({ "_tag": "FileNotFoundError", "path": "missing.txt", "message": "File not found: missing.txt" })
    );
    let r = h.post("/api/experimental/fs/watch", json!({ "path": "../outside.txt" })).await;
    assert_eq!(r.status, 404);
    let mut events = h.app.bus.subscribe();
    let r = h.post("/api/experimental/fs/watch", json!({ "path": "a.txt" })).await;
    assert_eq!(r.status, 204, "{}", String::from_utf8_lossy(&r.body));
    // Watching again only renews the expiry.
    assert_eq!(h.post("/api/experimental/fs/watch", json!({ "path": "a.txt" })).await.status, 204);
    let location = oc_schema::types::LocationRef { directory: h.dir(), workspace_id: None };
    assert_eq!(h.engine.file_watch.watched(&location), vec![h.cwd.join("a.txt")]);
    std::fs::write(h.cwd.join("a.txt"), "two\n").unwrap();
    let event = next_event(&mut events, "filesystem.changed").await;
    let file = h.cwd.join("a.txt").display().to_string();
    assert_eq!(event.data, json!({ "file": file, "event": "change" }));
    assert_eq!(event.location.as_ref().map(|l| l.directory.clone()), Some(h.dir()));
    std::fs::remove_file(h.cwd.join("a.txt")).unwrap();
    loop {
        let event = next_event(&mut events, "filesystem.changed").await;
        if event.data["event"] == "unlink" {
            assert_eq!(event.data["file"], json!(file));
            break;
        }
    }
    let r = h.req("DELETE", "/api/experimental/fs/watch", Some(json!({ "path": "a.txt" }))).await;
    assert_eq!(r.status, 204);
    assert!(h.engine.file_watch.watched(&location).is_empty());
    // Unwatching a file that is not watched is fine.
    let r = h.req("DELETE", "/api/experimental/fs/watch", Some(json!({ "path": "never.txt" }))).await;
    assert_eq!(r.status, 204);
}

#[tokio::test]
async fn file_watch_disabled_answers_service_unavailable() {
    let h = harness();
    let watch = oc_backend::file_watch::FileWatch::with_enabled(h.app.bus.clone(), false);
    let location = oc_schema::types::LocationRef { directory: h.dir(), workspace_id: None };
    let error = watch.watch(&location, &h.cwd.join("a.txt")).unwrap_err();
    assert_eq!(error.to_string(), "File watching is disabled");
}

#[cfg(unix)]
#[tokio::test]
async fn persistent_terminal_rejects_a_cwd_that_is_not_a_directory() {
    let h = harness();
    let id = h.session().await;
    let path = format!("/api/experimental/session/{id}/terminal");
    let file = h.cwd.join("file.txt");
    std::fs::write(&file, "").unwrap();
    for cwd in [h.cwd.join("missing"), file] {
        let cwd = cwd.display().to_string();
        let body = json!({ "command": "/bin/sh", "args": [], "cwd": cwd, "title": "invalid", "env": {} });
        let r = h.post(&path, body).await;
        assert_eq!(r.status, 400, "{}", String::from_utf8_lossy(&r.body));
        assert_eq!(
            r.json(),
            json!({ "_tag": "InvalidRequestError", "message": format!("cwd is not a directory: {cwd}"), "kind": "invalid_cwd", "field": "cwd" })
        );
    }
    let r = h.get(&path).await;
    assert_eq!(r.json(), json!({ "data": [] }));
}

fn insert_message(h: &Harness, session: &str, id: &str, kind: &str, seq: i64, data: Value) {
    h.app
        .db
        .with(|c| {
            c.execute(
                "INSERT INTO session_message (id, session_id, type, seq, time_created, time_updated, data) VALUES (?, ?, ?, ?, 0, 0, ?)",
                rusqlite::params![id, session, kind, seq, data.to_string()],
            )
        })
        .unwrap();
}

// session-diff.test.ts: `to` must be a user message, and messages are scoped to their Session.
#[tokio::test]
async fn session_diff_resolves_messages_by_point_lookups() {
    let h = harness();
    let first = h.session().await;
    let second = h.session().await;
    insert_message(&h, &first, "msg_user_1", "user", 1, json!({ "text": "hi" }));
    insert_message(&h, &first, "msg_step_1", "assistant", 2, json!({ "content": [], "time": { "created": 1 } }));
    insert_message(&h, &second, "msg_user_2", "user", 1, json!({ "text": "other" }));
    let r = h.get(&format!("/api/session/{first}/diff?from=msg_user_1&to=msg_step_1")).await;
    assert_eq!(r.status, 400, "{}", String::from_utf8_lossy(&r.body));
    assert_eq!(
        (r.json()["field"].clone(), r.json()["message"].clone()),
        (json!("to"), json!("Message msg_step_1 is not a user message"))
    );
    let r = h.get(&format!("/api/session/{first}/diff?from=msg_step_1")).await;
    assert_eq!(r.json()["field"], json!("from"));
    let r = h.get(&format!("/api/session/{first}/diff?from=msg_user_2")).await;
    assert_eq!(r.status, 404, "{}", String::from_utf8_lossy(&r.body));
    assert_eq!(r.json()["_tag"], "MessageNotFoundError");
    // A turn whose steps recorded no snapshot has no diff.
    let r = h.get(&format!("/api/session/{first}/diff")).await;
    assert_eq!((r.status.as_u16(), r.json()), (200, json!({ "data": [] })));
}

#[tokio::test]
async fn resolving_a_project_stores_its_icon_in_the_background() {
    if !git_available() {
        return;
    }
    let h = harness();
    let project = h.cwd.join("app");
    std::fs::create_dir_all(project.join("public")).unwrap();
    git(&project, &["init", "-q"]);
    git(&project, &["commit", "-q", "--allow-empty", "-m", "root"]);
    let svg = r#"<svg xmlns="http://www.w3.org/2000/svg"/>"#;
    std::fs::write(project.join("public/favicon.svg"), svg).unwrap();
    let mut events = h.app.bus.subscribe();
    let r = h.req_in(Some(&project.display().to_string()), "GET", "/api/location", None).await;
    assert_eq!(r.status, 200, "{}", String::from_utf8_lossy(&r.body));
    use base64::Engine as _;
    let url = format!("data:image/svg+xml;base64,{}", base64::engine::general_purpose::STANDARD.encode(svg));
    let event = loop {
        let event = next_event(&mut events, "project.updated").await;
        if event.data["icon"]["url"].is_string() {
            break event;
        }
    };
    assert_eq!(event.data["icon"], json!({ "url": url }));
    let r = h.get("/api/project").await;
    let listed = r
        .json()
        .as_array()
        .unwrap()
        .iter()
        .find(|p| p["id"] == event.data["id"])
        .cloned()
        .unwrap_or_else(|| panic!("{} not in {}", event.data, r.json()));
    assert_eq!(listed["icon"], json!({ "url": url }));
}
