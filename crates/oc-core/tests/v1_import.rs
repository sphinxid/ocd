//! V1 → V2 history import against a synthetic v1 database (tests/fixtures/v1_import).
//!
//! `schema.sql` is the schema of a fresh opencode 1.18.31 database (38 applied migrations); `data.sql` holds
//! hand-written v1 rows covering the import's edge cases (no user data). `expected.json` is what upstream
//! opencode v2 (`serve`, commit 4617210) wrote for exactly this input, with native data and a compaction's kept
//! text under their v2.1 names (`native`, `metadata.recent`; acdcdb8dd); every compared column must match exactly.

use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

use oc_core::v1_import::{Importer, Options, Status};
use oc_storage::Database;
use rusqlite::types::ValueRef;
use serde_json::{Map, Value, json};

const SCHEMA: &str = include_str!("fixtures/v1_import/schema.sql");
const DATA: &str = include_str!("fixtures/v1_import/data.sql");
const EXPECTED: &str = include_str!("fixtures/v1_import/expected.json");

fn v1_database(dir: &Path, data: &str) -> Database {
    let path = dir.join("opencode.db");
    {
        let c = rusqlite::Connection::open(&path).unwrap();
        c.execute_batch(SCHEMA).unwrap();
        c.execute_batch(data).unwrap();
    }
    // Applies the remaining v2 migrations (upgrade path), exactly like the server does at startup.
    Database::open(&path, dir).unwrap()
}

fn rows(db: &Database, sql: &str) -> Vec<Value> {
    db.with(|c| {
        let mut s = c.prepare(sql)?;
        let cols: Vec<String> = s.column_names().iter().map(|c| c.to_string()).collect();
        let mut out = Vec::new();
        let mut q = s.query([])?;
        while let Some(r) = q.next()? {
            let mut o = Map::new();
            for (i, col) in cols.iter().enumerate() {
                let v = match r.get_ref(i)? {
                    ValueRef::Null => Value::Null,
                    ValueRef::Integer(n) => json!(n),
                    ValueRef::Real(f) => json!(f),
                    ValueRef::Text(t) => json!(String::from_utf8_lossy(t)),
                    ValueRef::Blob(_) => json!("<blob>"),
                };
                o.insert(col.clone(), v);
            }
            out.push(Value::Object(o));
        }
        Ok(out)
    })
    .unwrap()
}

fn dump(db: &Database) -> Value {
    json!({
        "session_v2": rows(db, "SELECT * FROM session_v2 ORDER BY id"),
        "session_message": rows(db, "SELECT * FROM session_message ORDER BY session_id, seq"),
        "event_sequence": rows(db, "SELECT * FROM event_sequence ORDER BY aggregate_id"),
        "project": rows(db, "SELECT id, worktree, sandboxes FROM project ORDER BY id"),
        "kv": rows(db, "SELECT key, value FROM kv ORDER BY key"),
        "event_count": db.with(|c| c.query_row("SELECT COUNT(*) FROM event", [], |r| r.get::<_, i64>(0))).unwrap(),
    })
}

fn assert_same(actual: &Value, expected: &Value, path: &str) {
    match (actual, expected) {
        (Value::Object(a), Value::Object(e)) => {
            let ak: Vec<&String> = a.keys().collect();
            let ek: Vec<&String> = e.keys().collect();
            assert_eq!(ak, ek, "{path}: keys");
            for (k, v) in e {
                assert_same(&a[k], v, &format!("{path}.{k}"));
            }
        }
        (Value::Array(a), Value::Array(e)) => {
            assert_eq!(a.len(), e.len(), "{path}: length");
            for (i, (x, y)) in a.iter().zip(e).enumerate() {
                assert_same(x, y, &format!("{path}[{i}]"));
            }
        }
        (Value::Number(a), Value::Number(e)) => {
            // INTEGER stays INTEGER and REAL stays REAL; values compare exactly.
            assert_eq!(a.is_f64(), e.is_f64(), "{path}: storage class {a} vs {e}");
            assert_eq!(a.as_f64(), e.as_f64(), "{path}");
        }
        _ => assert_eq!(actual, expected, "{path}"),
    }
}

#[test]
fn imports_like_upstream() {
    let dir = tempfile::tempdir().unwrap();
    let db = v1_database(dir.path(), DATA);
    let importer = Importer::new();
    assert_eq!(importer.status(&db).unwrap(), Status::Required);
    importer.run(&db, dir.path(), &Options::default()).unwrap();
    assert_eq!(importer.status(&db).unwrap(), Status::Completed);
    let expected: Value = serde_json::from_str(EXPECTED).unwrap();
    assert_same(&dump(&db), &expected, "$");

    // A second run is a no-op once completed.
    importer.run(&db, dir.path(), &Options::default()).unwrap();
    assert_same(&dump(&db), &expected, "$");
}

#[test]
fn resumes_from_cursor() {
    let dir = tempfile::tempdir().unwrap();
    let db = v1_database(dir.path(), DATA);
    let importer = Importer::new();
    importer.run(&db, dir.path(), &Options::default()).unwrap();
    let expected = dump(&db);
    // Interrupted after ses_…003: the sessions before the cursor (higher ids) are done, the rest is redone.
    db.with(|c| {
        c.execute_batch(
            r#"UPDATE kv SET value = '{"phase":"sessions","cursor":"ses_fff000000003AAAAAAAAAAAAAA"}';
               DELETE FROM session_message WHERE session_id IN ('ses_fff000000001AAAAAAAAAAAAAA', 'ses_fff000000002AAAAAAAAAAAAAA');
               DELETE FROM session_v2 WHERE id = 'ses_fff000000001AAAAAAAAAAAAAA';
               UPDATE session_v2 SET title = 'kept' WHERE id = 'ses_fff000000003AAAAAAAAAAAAAA';"#,
        )
    })
    .unwrap();
    importer.run(&db, dir.path(), &Options::default()).unwrap();
    let mut after = dump(&db);
    let s3 = after["session_v2"].as_array_mut().unwrap().iter_mut().find(|s| s["id"] == "ses_fff000000003AAAAAAAAAAAAAA").unwrap();
    assert_eq!(s3["title"], "kept");
    s3["title"] = "Title AAAA".into();
    assert_same(&after, &expected, "$");
}

#[test]
fn fresh_database_needs_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let db = Database::open(&dir.path().join("opencode.db"), dir.path()).unwrap();
    let importer = Importer::new();
    assert_eq!(importer.status(&db).unwrap(), Status::Completed);
    importer.run(&db, dir.path(), &Options::default()).unwrap();
    assert_eq!(rows(&db, "SELECT key FROM kv WHERE key = 'migration.v1-v2'").len(), 0);
}

#[test]
fn background_start_reports_progress_and_errors() {
    // A malformed percent-encoded data: URL throws in upstream (`decodeURIComponent`) and fails the import.
    let bad = r#"
        INSERT INTO session (id, project_id, slug, directory, title, version, time_created, time_updated)
          VALUES ('ses_bad', 'global', 's', '/', 't', '1', 1, 2);
        INSERT INTO message (id, session_id, time_created, time_updated, data) VALUES ('msg_bad', 'ses_bad', 1, 2,
          '{"role":"user","time":{"created":1},"agent":"build","model":{"providerID":"p","modelID":"m"}}');
        INSERT INTO part (id, message_id, session_id, time_created, time_updated, data) VALUES ('prt_bad', 'msg_bad', 'ses_bad', 1, 1,
          '{"type":"file","mime":"text/plain","url":"data:text/plain,%E0%A4%A"}');
    "#;
    let dir = tempfile::tempdir().unwrap();
    let db = v1_database(dir.path(), &format!("{DATA}\n{bad}"));
    let importer: Arc<Importer> = Importer::new();
    let handle = importer.start(db.clone(), dir.path().to_path_buf());
    match importer.status(&db).unwrap() {
        Status::Running(_) | Status::Error(_) => {}
        other => panic!("unexpected status {other:?}"),
    }
    handle.join().unwrap();
    assert_eq!(importer.status(&db).unwrap(), Status::Error("URI error".into()));
    assert_eq!(importer.status(&db).unwrap().to_json(), json!({"status": "error", "error": "URI error"}));
    // Sessions after the failing one were not imported; the cursor points at it, the transaction rolled back.
    let kv = rows(&db, "SELECT value FROM kv WHERE key = 'migration.v1-v2'");
    assert_eq!(kv[0]["value"], r#"{"phase":"sessions","cursor":"ses_fff000000001AAAAAAAAAAAAAA"}"#);
    assert!(rows(&db, "SELECT id FROM session_v2 WHERE id = 'ses_bad'").is_empty());

    // A clean database completes in the background.
    let dir = tempfile::tempdir().unwrap();
    let db = v1_database(dir.path(), DATA);
    let importer = Importer::new();
    let handle = importer.start(db.clone(), dir.path().to_path_buf());
    let deadline = Instant::now() + Duration::from_secs(30);
    while importer.status(&db).unwrap() != Status::Completed {
        assert!(Instant::now() < deadline, "import did not finish");
        std::thread::sleep(Duration::from_millis(5));
    }
    handle.join().unwrap();
    assert_eq!(
        Status::Running(oc_core::v1_import::Progress { label: "Migrating sessions".into(), numerator: Some(3), denominator: Some(5) })
            .to_json(),
        json!({"status": "running", "progress": {"label": "Migrating sessions", "numerator": 3, "denominator": 5}})
    );
}

/// A v1 database with only the given rows (no fixture data).
fn empty_v1(dir: &Path, rows: &str) -> Database {
    v1_database(dir, rows)
}

// v1-migration.test.ts "migrates $directory in $worktree": legacy ACP sessions can store a cwd relative to an
// unknown process directory; the project worktree replaces it and the V1 row stays intact.
#[test]
fn migrates_relative_session_directories() {
    for (worktree, directory, expected) in [
        ("/repo", ".", "/repo"),
        ("/repo", "src", "/repo"),
        ("C:/repo", ".", "C:/repo"),
        ("//server/share/repo", ".", "//server/share/repo"),
        ("/repo", "C:/other", "C:/other"),
        ("/repo", "//server/share/other", "//server/share/other"),
        ("/repo", "", ""),
        ("relative", ".", ""),
    ] {
        let dir = tempfile::tempdir().unwrap();
        let db = empty_v1(
            dir.path(),
            &format!(
                "INSERT INTO project (id, worktree, time_created, time_updated, sandboxes) VALUES ('saved-project', '{worktree}', 1, 2, '[]');
                 INSERT INTO session (id, project_id, slug, directory, title, version, time_created, time_updated)
                   VALUES ('ses_test', 'saved-project', 'test', '{directory}', 'Test', '1', 1, 2);"
            ),
        );
        Importer::new().run(&db, dir.path(), &Options::default()).unwrap();
        assert_eq!(rows(&db, "SELECT directory FROM session_v2 WHERE id = 'ses_test'"), vec![json!({ "directory": expected })], "{worktree} {directory}");
        assert_eq!(rows(&db, "SELECT directory FROM session WHERE id = 'ses_test'"), vec![json!({ "directory": directory })]);
    }
}

// "reassigns V1 sessions with missing projects and directory %s"
#[test]
fn reassigns_sessions_with_missing_projects() {
    for directory in ["/nonexistent-ocd-v1-orphan", "."] {
        let dir = tempfile::tempdir().unwrap();
        let db = empty_v1(
            dir.path(),
            &format!(
                "INSERT INTO session (id, project_id, slug, directory, title, version, time_created, time_updated)
                   VALUES ('ses_orphan', 'missing-project', 'orphan', '{directory}', 'Orphan', '1', 1, 2);"
            ),
        );
        Importer::new().run(&db, dir.path(), &Options::default()).unwrap();
        assert_eq!(rows(&db, "SELECT project_id FROM session_v2 WHERE id = 'ses_orphan'"), vec![json!({ "project_id": "global" })]);
        assert_eq!(rows(&db, "SELECT worktree FROM project WHERE id = 'global'"), vec![json!({ "worktree": "/" })]);
        let expected = if directory == "." { "/" } else { directory };
        assert_eq!(rows(&db, "SELECT directory FROM session_v2 WHERE id = 'ses_orphan'"), vec![json!({ "directory": expected })]);
        assert_eq!(rows(&db, "SELECT value FROM kv WHERE key = 'migration.v1-v2'"), vec![json!({ "value": r#"{"phase":"completed"}"# })]);
    }
}

fn git(dir: &Path, args: &[&str]) {
    let status = std::process::Command::new("git")
        .args(["-c", "init.defaultBranch=main", "-c", "commit.gpgsign=false", "-c", "user.name=t", "-c", "user.email=t@t"])
        .args(args)
        .current_dir(dir)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .unwrap();
    assert!(status.success());
}

// "attributes V1 global sessions to the V2 project of their directory"
#[tokio::test]
async fn attributes_global_sessions_to_their_directory_projects() {
    let tmp = tempfile::tempdir().unwrap();
    let root = std::fs::canonicalize(tmp.path()).unwrap();
    let plain = root.join("plain");
    let nested = plain.join("nested");
    let repo = root.join("repo");
    let missing = root.join("missing");
    std::fs::create_dir_all(&nested).unwrap();
    std::fs::create_dir_all(repo.join("pkg/a")).unwrap();
    std::fs::write(repo.join("README.md"), "repo").unwrap();
    git(&repo, &["init", "-q"]);
    git(&repo, &["add", "-A"]);
    git(&repo, &["commit", "-q", "-m", "initial"]);
    let data = tempfile::tempdir().unwrap();
    let mut inserts = String::from("INSERT INTO project (id, worktree, time_created, time_updated, sandboxes) VALUES ('global', '/', 1, 2, '[]');\n");
    for (id, project, directory) in [
        ("ses_plain", "global", plain.clone()),
        ("ses_nested", "global", nested.clone()),
        ("ses_repo", "global", repo.join("pkg/a")),
        ("ses_missing", "global", missing.clone()),
        ("ses_orphan", "missing-project", plain.clone()),
        ("ses_imported", "global", plain.clone()),
    ] {
        inserts.push_str(&format!(
            "INSERT INTO session (id, project_id, slug, directory, title, version, time_created, time_updated) VALUES ('{id}', '{project}', '{id}', '{}', 'Test', '1', 1, 2);\n",
            directory.display()
        ));
    }
    let db = empty_v1(data.path(), &inserts);
    db.with(|c| {
        c.execute(
            "INSERT INTO session_v2 (id, project_id, slug, directory, title, version, time_created, time_updated) VALUES ('ses_imported', 'global', 'ses_imported', ?1, 'Kept', '1', 1, 2)",
            [plain.display().to_string()],
        )
    })
    .unwrap();
    let path = data.path().to_path_buf();
    let db2 = db.clone();
    tokio::task::spawn_blocking(move || Importer::new().run(&db2, &path, &Options::default()).unwrap()).await.unwrap();
    // New owners take their recency from the V1 session instead of the import time.
    assert_eq!(
        rows(&db, "SELECT time_active FROM project WHERE id NOT IN ('global', 'missing-project')"),
        vec![json!({ "time_active": 2 }); 3]
    );
    let plain_project = oc_core::project::resolve(&plain).await;
    let nested_project = oc_core::project::resolve(&nested).await;
    let repo_project = oc_core::project::resolve(&repo).await;
    assert_eq!(repo_project.vcs.as_ref().map(|v| v.kind()), Some("git"));
    assert_eq!(
        rows(&db, "SELECT id, project_id, path, title FROM session_v2 ORDER BY id"),
        vec![
            json!({ "id": "ses_imported", "project_id": "global", "path": null, "title": "Kept" }),
            json!({ "id": "ses_missing", "project_id": "global", "path": null, "title": "Test" }),
            json!({ "id": "ses_nested", "project_id": nested_project.id, "path": "", "title": "Test" }),
            json!({ "id": "ses_orphan", "project_id": plain_project.id, "path": "", "title": "Test" }),
            json!({ "id": "ses_plain", "project_id": plain_project.id, "path": "", "title": "Test" }),
            json!({ "id": "ses_repo", "project_id": repo_project.id, "path": "pkg/a", "title": "Test" }),
        ]
    );
}

