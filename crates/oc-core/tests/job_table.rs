//! Background job markers in the SQLite `job` table (`core/test/job.test.ts`, 2.2.4+).

use std::sync::Arc;

use oc_core::job::Jobs;
use oc_storage::Database;
use oc_tools::host::{JobRecovery, JobStatus};
use serde_json::{Value, json};

fn shell(session: &str, id: &str, command: &str) -> JobRecovery {
    JobRecovery::Shell { session_id: session.into(), shell_id: id.into(), command: command.into() }
}

fn rows(db: &Database) -> Vec<(String, String, String, Option<String>, String, Option<String>, String)> {
    db.with(|c| {
        let mut s = c.prepare(
            "SELECT notification_id, id, session_id, child_session_id, kind, title, status FROM job ORDER BY time_created, notification_id",
        )?;
        let r = s
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?, r.get(6)?)))?
            .collect();
        r
    })
    .unwrap()
}

fn find<'a>(pending: &'a [Value], id: &str) -> Option<&'a Value> {
    pending.iter().find(|v| v["id"] == id)
}

#[tokio::test]
async fn retains_background_ownership_and_terminal_output_until_notification_acknowledgment() {
    let db = Database::memory().unwrap();
    let jobs = Arc::new(Jobs::new(db.clone()));
    let (tx, rx) = tokio::sync::oneshot::channel::<()>();
    let recovery = shell("ses_background_shell", "shell_background", "echo done");
    let job = jobs.start(
        "shell_background".into(),
        "shell".into(),
        None,
        Some(recovery.clone()),
        Box::pin(async move {
            let _ = rx.await;
            Ok("done".to_string())
        }),
    );
    assert!(find(&jobs.pending_background(), &job.id).is_none());
    let background = jobs.background(&job.id).unwrap();
    let pending = jobs.pending_background();
    let running = find(&pending, &job.id).unwrap();
    assert_eq!(running["status"], "running");
    assert_eq!(running["recovery"], serde_json::to_value(&recovery).unwrap());
    let nid = running["notificationID"].as_str().unwrap().to_string();
    assert!(nid.starts_with("msg_"));
    assert_eq!(background.notification_id.as_deref(), Some(nid.as_str()));
    // The owner is the session, the title the command, the creation time the job's start.
    let row = rows(&db).remove(0);
    assert_eq!(row, (nid.clone(), job.id.clone(), "ses_background_shell".into(), None, "shell".into(), Some("echo done".into()), "running".into()));
    let created: i64 = db.with(|c| c.query_row("SELECT time_created FROM job", [], |r| r.get(0))).unwrap();
    assert_eq!(created, job.started_at as i64);

    tx.send(()).unwrap();
    jobs.wait(&job.id).await;
    let pending = jobs.pending_background();
    assert_eq!(
        find(&pending, &job.id).unwrap(),
        &json!({"id": job.id, "notificationID": nid, "recovery": recovery, "status": "completed", "output": "done"})
    );
    // The upsert keeps the creation time.
    let created_after: i64 = db.with(|c| c.query_row("SELECT time_created FROM job", [], |r| r.get(0))).unwrap();
    assert_eq!(created_after, created);
    jobs.complete_background(&nid);
    assert!(find(&jobs.pending_background(), &job.id).is_none());
    assert!(rows(&db).is_empty());
}

#[tokio::test]
async fn persists_subagent_ownership_by_parent_and_cancellation() {
    let db = Database::memory().unwrap();
    let jobs = Arc::new(Jobs::new(db.clone()));
    let recovery = JobRecovery::Subagent {
        parent_session_id: "ses_background_parent".into(),
        child_session_id: "ses_background_child".into(),
        agent: "explore".into(),
        description: "Explore background recovery".into(),
    };
    let job = jobs.start(
        "ses_background_child".into(),
        "subagent".into(),
        None,
        Some(recovery.clone()),
        Box::pin(futures::future::pending()),
    );
    let blocked = {
        let jobs = jobs.clone();
        let id = job.id.clone();
        tokio::spawn(async move { jobs.block(&id, "ses_background_parent").await })
    };
    tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    assert_eq!(jobs.background_all("ses_background_parent", None).len(), 1);
    assert!(matches!(blocked.await.unwrap(), Some(oc_tools::host::JobBlockResult::Backgrounded(_))));
    let marker = find(&jobs.pending_background(), &job.id).cloned().unwrap();
    assert_eq!(marker["status"], "running");
    let row = rows(&db).remove(0);
    assert_eq!(row.2, "ses_background_parent");
    assert_eq!(row.3.as_deref(), Some("ses_background_child"));
    assert_eq!(row.5.as_deref(), Some("Explore background recovery"));
    jobs.cancel(&job.id);
    let after = find(&jobs.pending_background(), &job.id).cloned().unwrap();
    assert_eq!(after["notificationID"], marker["notificationID"]);
    assert_eq!(after["status"], "cancelled");
}

#[tokio::test]
async fn retains_terminal_errors_for_recovery_until_notification_acknowledgment() {
    let db = Database::memory().unwrap();
    let jobs = Arc::new(Jobs::new(db.clone()));
    let job = jobs.start(
        "shell_immediate_error".into(),
        "shell".into(),
        None,
        Some(shell("ses_immediate_error", "shell_immediate_error", "exit 1")),
        Box::pin(async { Err("shell failed".to_string()) }),
    );
    assert_eq!(jobs.wait(&job.id).await.unwrap().status, JobStatus::Error);
    let background = jobs.background(&job.id).unwrap();
    let pending = jobs.pending_background();
    assert_eq!(pending.len(), 1);
    assert_eq!(pending[0]["notificationID"].as_str(), background.notification_id.as_deref());
    assert_eq!(pending[0]["status"], "error");
    assert_eq!(pending[0]["error"], "shell failed");
}

#[tokio::test]
async fn recovers_a_background_marker_after_its_process_local_registry_closes() {
    let db = Database::memory().unwrap();
    let previous = Arc::new(Jobs::new(db.clone()));
    let job = previous.start(
        "shell_restart".into(),
        "shell".into(),
        None,
        Some(shell("ses_background_restart", "shell_restart", "sleep 60")),
        Box::pin(futures::future::pending()),
    );
    previous.background(&job.id);
    drop(previous);
    let current = Jobs::new(db);
    let marker = find(&current.pending_background(), &job.id).cloned().unwrap();
    assert_eq!(marker["status"], "running");
    current.complete_background(marker["notificationID"].as_str().unwrap());
    assert!(current.pending_background().is_empty());
}

#[tokio::test]
async fn cancels_running_session_jobs_and_removes_unacknowledged_rows_on_cancel_session() {
    let db = Database::memory().unwrap();
    let jobs = Arc::new(Jobs::new(db.clone()));
    let (dropped_tx, dropped_rx) = tokio::sync::oneshot::channel::<()>();
    struct Guard(Option<tokio::sync::oneshot::Sender<()>>);
    impl Drop for Guard {
        fn drop(&mut self) {
            if let Some(tx) = self.0.take() {
                let _ = tx.send(());
            }
        }
    }
    let owned = jobs.start(
        "job_cleanup_owned".into(),
        "shell".into(),
        None,
        Some(shell("ses_cleanup_owner", "job_cleanup_owned", "bun run dev")),
        Box::pin(async move {
            let _guard = Guard(Some(dropped_tx));
            futures::future::pending::<()>().await;
            Ok(String::new())
        }),
    );
    let other = jobs.start(
        "job_cleanup_other".into(),
        "shell".into(),
        None,
        Some(shell("ses_cleanup_other", "job_cleanup_other", "bun test")),
        Box::pin(async { Ok("kept".to_string()) }),
    );
    jobs.background(&owned.id);
    jobs.background(&other.id);
    jobs.wait(&other.id).await;
    let cancelled = jobs.cancel_session("ses_cleanup_owner");
    assert_eq!(cancelled.len(), 1);
    assert_eq!(cancelled[0].id, owned.id);
    assert_eq!(cancelled[0].status, JobStatus::Cancelled);
    tokio::time::timeout(std::time::Duration::from_secs(1), dropped_rx).await.unwrap().unwrap();
    assert!(rows(&db).iter().all(|r| r.2 != "ses_cleanup_owner"));
    let remaining = jobs.pending_background();
    assert_eq!(remaining.len(), 1);
    assert_eq!(remaining[0]["id"], other.id);
    assert_eq!(remaining[0]["status"], "completed");
    assert_eq!(remaining[0]["output"], "kept");
    // The cancelled job is forgotten, not kept as a completed entry.
    assert!(jobs.get(&owned.id).is_none());
}

#[tokio::test]
async fn cancel_session_matches_a_subagent_by_its_child_and_deletes_child_rows() {
    let db = Database::memory().unwrap();
    let jobs = Arc::new(Jobs::new(db.clone()));
    let job = jobs.start(
        "ses_child".into(),
        "subagent".into(),
        None,
        Some(JobRecovery::Subagent {
            parent_session_id: "ses_parent".into(),
            child_session_id: "ses_child".into(),
            agent: "general".into(),
            description: "d".into(),
        }),
        Box::pin(futures::future::pending()),
    );
    jobs.background(&job.id);
    assert_eq!(rows(&db).len(), 1);
    assert_eq!(jobs.cancel_session("ses_child").len(), 1);
    assert!(rows(&db).is_empty());
    assert!(jobs.get("ses_child").is_none());
}
