//! Background job markers in the SQLite `job` table (`core/test/job.test.ts`, 2.2.4+).

use std::sync::Arc;

use oc_core::job::{Jobs, SubagentOutboxReport, SubagentReportState};
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
        outstanding: None,
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
            outstanding: None,
        }),
        Box::pin(futures::future::pending()),
    );
    jobs.background(&job.id);
    assert_eq!(rows(&db).len(), 1);
    assert_eq!(jobs.cancel_session("ses_child").len(), 1);
    assert!(rows(&db).is_empty());
    assert!(jobs.get("ses_child").is_none());
}

fn subagent(parent: &str, child: &str, description: &str) -> JobRecovery {
    JobRecovery::Subagent { parent_session_id: parent.into(), child_session_id: child.into(), agent: "explore".into(), description: description.into(), outstanding: None }
}

fn background(id: &str, recovery: JobRecovery, notification_id: Option<&str>, run: futures::future::BoxFuture<'static, Result<String, String>>) -> oc_core::job::StartInput {
    oc_core::job::StartInput {
        id: id.into(),
        kind: "subagent".into(),
        title: None,
        metadata_session: None,
        recovery: Some(recovery),
        notification_id: notification_id.map(str::to_string),
        background: true,
        run,
    }
}

/// job.test.ts "does not restore a consumed background row when its running work settles" (2.3.0).
#[tokio::test]
async fn does_not_restore_a_consumed_background_row_when_its_running_work_settles() {
    let db = Database::memory().unwrap();
    let jobs = Arc::new(Jobs::new(db.clone()));
    let (tx, rx) = tokio::sync::oneshot::channel::<()>();
    let job = jobs.start(
        "shell_consumed_before_settle".into(),
        "shell".into(),
        None,
        Some(shell("ses_consumed_before_settle", "shell_consumed_before_settle", "bun test")),
        Box::pin(async move {
            let _ = rx.await;
            Ok("done".to_string())
        }),
    );
    let nid = jobs.background(&job.id).unwrap().notification_id.unwrap();
    let waiting = {
        let jobs = jobs.clone();
        let id = job.id.clone();
        tokio::spawn(async move { jobs.wait(&id).await })
    };
    // The waiter observes the job before its notice is acknowledged.
    tokio::task::yield_now().await;
    jobs.complete_background(&nid);
    assert!(jobs.pending_background().is_empty());
    assert!(jobs.background_jobs().is_empty());
    tx.send(()).unwrap();
    let info = waiting.await.unwrap().unwrap();
    assert_eq!((info.status, info.output.as_deref()), (JobStatus::Completed, Some("done")));
    // Settling a consumed job must not re-insert the row its notification already cleared.
    assert!(jobs.pending_background().is_empty());
    assert!(jobs.background_jobs().is_empty());
    assert!(jobs.get(&job.id).is_none());
}

/// "classifies a backgrounded subagent report and marks it settling"
#[tokio::test]
async fn classifies_a_backgrounded_subagent_report_and_marks_it_settling() {
    let db = Database::memory().unwrap();
    let jobs = Arc::new(Jobs::new(db.clone()));
    let (tx, rx) = tokio::sync::oneshot::channel::<()>();
    let recovery = subagent("ses_report_parent", "ses_report_child", "Report work");
    let mut input = background(
        "ses_report_child",
        recovery.clone(),
        None,
        Box::pin(async move {
            let _ = rx.await;
            Ok("done".to_string())
        }),
    );
    input.title = Some("Report work".into());
    let job = jobs.start_input(input);
    let nid = job.notification_id.clone().unwrap();
    assert!(jobs.current_generation(&job.id).unwrap().1);
    assert_eq!(
        jobs.prepare_subagent_report(&job.id, None),
        SubagentReportState::Background { notification_id: nid.clone(), recovery: recovery.clone(), started_at: job.started_at }
    );
    assert!(!jobs.current_generation(&job.id).unwrap().1);
    assert_eq!(jobs.prepare_subagent_report(&job.id, None), SubagentReportState::None);
    assert!(jobs.current_generation("job_missing").is_none());
    tx.send(()).unwrap();
    jobs.wait(&job.id).await;
    // The settlement commit owns the terminal row while this generation is settling.
    let pending = jobs.pending_background();
    assert_eq!((pending[0]["notificationID"].as_str(), pending[0]["status"].as_str()), (Some(nid.as_str()), Some("running")));
    let report = SubagentOutboxReport {
        notification_id: nid.clone(),
        recovery,
        status: JobStatus::Completed,
        output: Some("done".into()),
        error: None,
        started_at: Some(job.started_at),
    };
    db.with(|c| Jobs::record_subagent_report(c, &report)).unwrap();
    let pending = jobs.pending_background();
    assert_eq!((pending[0]["status"].as_str(), pending[0]["output"].as_str()), (Some("completed"), Some("done")));
    jobs.complete_background(&nid);
}

/// "starts a fresh generation while the previous subagent report is settling"
#[tokio::test]
async fn starts_a_fresh_generation_while_the_previous_subagent_report_is_settling() {
    let db = Database::memory().unwrap();
    let jobs = Arc::new(Jobs::new(db));
    let recovery = subagent("ses_settling_parent", "ses_settling_child", "Settling work");
    let (old_tx, old_rx) = tokio::sync::oneshot::channel::<()>();
    let job = jobs.start_input(background(
        "ses_settling_child",
        recovery.clone(),
        None,
        Box::pin(async move {
            let _ = old_rx.await;
            Ok("old".to_string())
        }),
    ));
    let (generation, _) = jobs.current_generation(&job.id).unwrap();
    let old_wait = {
        let jobs = jobs.clone();
        let id = job.id.clone();
        tokio::spawn(async move { jobs.wait(&id).await })
    };
    tokio::task::yield_now().await;
    assert!(matches!(jobs.prepare_subagent_report(&job.id, None), SubagentReportState::Background { .. }));
    assert_eq!(jobs.current_generation(&job.id), Some((generation, false)));
    let (new_tx, new_rx) = tokio::sync::oneshot::channel::<String>();
    jobs.start_input(background("ses_settling_child", recovery, None, Box::pin(async move { Ok(new_rx.await.unwrap()) })));
    let current = jobs.current_generation(&job.id).unwrap();
    assert_ne!(current.0, generation);
    assert!(current.1);
    old_tx.send(()).unwrap();
    // The superseded generation still resolves its waiters instead of dropping the settlement.
    let old = old_wait.await.unwrap().unwrap();
    assert_eq!((old.status, old.output.as_deref()), (JobStatus::Completed, Some("old")));
    assert_eq!(jobs.get(&job.id).unwrap().status, JobStatus::Running);
    new_tx.send("new".into()).unwrap();
    let new = jobs.wait(&job.id).await.unwrap();
    assert_eq!((new.status, new.output.as_deref()), (JobStatus::Completed, Some("new")));
}

/// "matches an external cancel to its busy period through the bound generation"
#[tokio::test]
async fn matches_an_external_cancel_to_its_busy_period_through_the_bound_generation() {
    let jobs = Arc::new(Jobs::new(Database::memory().unwrap()));
    let job = jobs.start_input(background(
        "ses_cancel_bound_child",
        subagent("ses_cancel_bound_parent", "ses_cancel_bound_child", "Cancelled during the busy period"),
        None,
        Box::pin(futures::future::pending()),
    ));
    let (generation, _) = jobs.current_generation(&job.id).unwrap();
    jobs.cancel(&job.id);
    assert_eq!(jobs.get(&job.id).unwrap().status, JobStatus::Cancelled);
    // The cancel consumed the live entry; its matching generation still belongs to this busy period.
    assert_eq!(jobs.prepare_subagent_report(&job.id, Some(generation - 1)), SubagentReportState::None);
    assert!(matches!(jobs.prepare_subagent_report(&job.id, Some(generation)), SubagentReportState::Background { .. }));
    assert_eq!(jobs.prepare_subagent_report(&job.id, None), SubagentReportState::None);
}

/// "persists a background marker before forking background work"
#[tokio::test]
async fn persists_a_background_marker_before_forking_background_work() {
    let db = Database::memory().unwrap();
    let jobs = Arc::new(Jobs::new(db));
    let probe = jobs.clone();
    let job = jobs.start_input(background(
        "ses_atomic_child",
        subagent("ses_atomic_parent", "ses_atomic_child", "Atomic start"),
        Some("msg_atomic"),
        Box::pin(async move {
            // The row is already durable when the run begins.
            Ok(probe.pending_background().iter().any(|r| r["notificationID"] == "msg_atomic" && r["status"] == "running").to_string())
        }),
    ));
    assert_eq!(job.notification_id.as_deref(), Some("msg_atomic"));
    let info = jobs.wait(&job.id).await.unwrap();
    assert_eq!((info.status, info.output.as_deref()), (JobStatus::Completed, Some("true")));
    let pending = jobs.pending_background();
    assert_eq!((pending[0]["status"].as_str(), pending[0]["output"].as_str()), (Some("completed"), Some("true")));
    jobs.complete_background("msg_atomic");
}

/// "distinguishes caller cancellation from teardown" (the explicit half: a registry dropped without cancelling
/// leaves nothing cancelled, which `recovers_a_background_marker_after_its_process_local_registry_closes` covers).
#[tokio::test]
async fn distinguishes_caller_cancellation() {
    let jobs = Arc::new(Jobs::new(Database::memory().unwrap()));
    let (dropped_tx, dropped_rx) = tokio::sync::oneshot::channel::<()>();
    struct Guard(Option<tokio::sync::oneshot::Sender<()>>);
    impl Drop for Guard {
        fn drop(&mut self) {
            let _ = self.0.take().map(|tx| tx.send(()));
        }
    }
    let job = jobs.start(
        "job_explicit_cancel".into(),
        "test".into(),
        None,
        None,
        Box::pin(async move {
            let _guard = Guard(Some(dropped_tx));
            futures::future::pending::<()>().await;
            Ok(String::new())
        }),
    );
    assert!(!jobs.is_cancelled(&job.id));
    // The run has started before it is cancelled.
    tokio::task::yield_now().await;
    jobs.cancel(&job.id);
    assert!(jobs.is_cancelled(&job.id));
    tokio::time::timeout(std::time::Duration::from_secs(1), dropped_rx).await.unwrap().unwrap();
    // A session teardown (`cancelSession`) is not a caller cancellation.
    let other = jobs.start("job_session_cancel".into(), "test".into(), None, Some(shell("ses_teardown", "job_session_cancel", "x")), Box::pin(futures::future::pending()));
    let generation = jobs.current_generation(&other.id).unwrap().0;
    assert!(!jobs.is_cancelled_generation(&other.id, generation));
}

/// "matches a creation-tick cancellation without a bound generation exactly once"
#[tokio::test]
async fn matches_a_creation_tick_cancellation_without_a_bound_generation_exactly_once() {
    let jobs = Arc::new(Jobs::new(Database::memory().unwrap()));
    jobs.start_input(background(
        "ses_creation_child",
        subagent("ses_creation_parent", "ses_creation_child", "Cancelled on creation"),
        None,
        Box::pin(futures::future::pending()),
    ));
    jobs.cancel("ses_creation_child");
    assert!(matches!(jobs.prepare_subagent_report("ses_creation_child", None), SubagentReportState::Background { .. }));
    assert_eq!(jobs.prepare_subagent_report("ses_creation_child", None), SubagentReportState::None);
}

/// "excludes settling and consumed work from running background snapshots"
#[tokio::test]
async fn excludes_settling_and_consumed_work_from_running_background_snapshots() {
    let db = Database::memory().unwrap();
    let jobs = Arc::new(Jobs::new(db.clone()));
    let recovery = subagent("ses_snapshot_parent", "ses_snapshot_child", "Snapshot work");
    let (tx, rx) = tokio::sync::oneshot::channel::<()>();
    let job = jobs.start_input(background(
        "ses_snapshot_child",
        recovery.clone(),
        None,
        Box::pin(async move {
            let _ = rx.await;
            Ok("done".to_string())
        }),
    ));
    let nid = job.notification_id.clone().unwrap();
    assert_eq!(jobs.running_background(), vec![recovery]);
    assert!(matches!(jobs.prepare_subagent_report("ses_snapshot_child", None), SubagentReportState::Background { .. }));
    assert!(jobs.running_background().is_empty());
    // A settling live entry without its row must not leak back into the durable list.
    db.with(|c| Jobs::delete_background_row(c, &nid)).unwrap();
    assert!(jobs.background_jobs().is_empty());
    tx.send(()).unwrap();
    jobs.wait("ses_snapshot_child").await;
    jobs.complete_background(&nid);
    assert!(jobs.running_background().is_empty());
}

/// `Job.BlockingParent` (2.3.1, #64343): a wait inside a background Code Mode script does not hold the session, so a
/// session-wide background request moves the script and leaves the nested job waiting with its own deadline; the
/// script's row is a `codemode` job titled after the script.
#[tokio::test]
async fn a_session_wide_background_request_moves_the_script_and_not_its_nested_waits() {
    let db = Database::memory().unwrap();
    let jobs = Arc::new(Jobs::new(db.clone()));
    let session = "ses_codemode_parent";
    let script = jobs.start(
        "cm_script".into(),
        "codemode".into(),
        Some("Code Mode script".into()),
        Some(JobRecovery::Codemode { session_id: session.into(), job_id: "cm_script".into() }),
        Box::pin(futures::future::pending()),
    );
    let nested = jobs.start(
        "sh_nested".into(),
        "shell".into(),
        None,
        Some(shell(session, "sh_nested", "sleep 100")),
        Box::pin(futures::future::pending()),
    );
    let waits = [(script.id.clone(), None), (nested.id.clone(), Some(script.id.clone()))].map(|(id, parent)| {
        let jobs = jobs.clone();
        tokio::spawn(async move { jobs.block_under(&id, session, parent.as_deref()).await })
    });
    tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    let moved = jobs.background_all(session, None);
    assert_eq!(moved.iter().map(|j| j.id.as_str()).collect::<Vec<_>>(), ["cm_script"]);
    let [script_wait, nested_wait] = waits;
    assert!(matches!(script_wait.await.unwrap(), Some(oc_tools::host::JobBlockResult::Backgrounded(_))));
    assert!(!nested_wait.is_finished());
    assert_eq!(
        rows(&db),
        [(
            moved[0].notification_id.clone().unwrap(),
            "cm_script".into(),
            session.into(),
            None,
            "codemode".into(),
            Some("Code Mode script".into()),
            "running".into()
        )]
    );
    assert_eq!(
        find(&jobs.pending_background(), "cm_script").unwrap()["recovery"],
        json!({ "kind": "codemode", "sessionID": session, "jobID": "cm_script" })
    );
    jobs.cancel(&nested.id);
    assert!(matches!(nested_wait.await.unwrap(), Some(oc_tools::host::JobBlockResult::Finished(_))));
}
