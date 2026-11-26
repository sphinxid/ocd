//! `POST /api/experimental/job/{id}/cancel` and the ephemeral `experimental.job.updated` event (2.4.1, #64790):
//! clients follow listed jobs through the event, and cancel one through the route.

mod common;

use std::time::Duration;

use common::*;
use oc_core::job::StartInput;
use oc_tools::host::JobRecovery;
use serde_json::{Value, json};

/// The next `experimental.job.updated` event's `info`.
async fn next_update(rx: &mut tokio::sync::broadcast::Receiver<oc_core::bus::Event>) -> Value {
    loop {
        let event = tokio::time::timeout(Duration::from_secs(10), rx.recv()).await.expect("an update").unwrap();
        if event.kind == "experimental.job.updated" {
            assert!(event.location.is_none());
            return event.data["info"].clone();
        }
    }
}

fn subset(actual: &Value, expected: Value) {
    for (key, value) in expected.as_object().unwrap() {
        assert_eq!(actual.get(key), Some(value), "{key} in {actual}");
    }
}

#[tokio::test]
async fn cancels_jobs_and_publishes_their_updates() {
    let h = harness();
    let session = h.session().await;
    let mut rx = h.app.bus.subscribe();
    let shell = |id: &str| JobRecovery::Shell { session_id: session.clone(), shell_id: format!("sh_{id}"), command: "sleep 100".into() };
    let start = |id: &str, background: bool, run: futures::future::BoxFuture<'static, Result<String, String>>| {
        h.app.jobs.start_input(StartInput {
            id: id.into(),
            kind: "shell".into(),
            title: None,
            metadata_session: None,
            recovery: Some(shell(id)),
            notification_id: None,
            background,
            run,
        })
    };

    // A job started in the background is listed right away.
    start("job_bg", true, Box::pin(futures::future::pending()));
    let info = next_update(&mut rx).await;
    assert_eq!(info.as_object().unwrap().keys().collect::<Vec<_>>(), ["kind", "id", "sessionID", "status", "time", "shellID", "command"]);
    subset(&info, json!({"kind": "shell", "id": "job_bg", "sessionID": session, "status": "running", "shellID": "sh_job_bg", "command": "sleep 100"}));

    // Cancelling it through the route interrupts it and lists it as cancelled.
    let cancelled = h.post("/api/experimental/job/job_bg/cancel", json!({})).await;
    assert_eq!(cancelled.status.as_u16(), 204);
    assert!(cancelled.body.is_empty());
    let info = next_update(&mut rx).await;
    subset(&info, json!({"id": "job_bg", "status": "cancelled"}));
    assert!(info["time"]["completed"].is_u64());
    assert_eq!(h.app.jobs.get("job_bg").map(|job| job.status), Some(oc_tools::host::JobStatus::Cancelled));

    // Cancelling a finished or unknown job does nothing.
    assert_eq!(h.post("/api/experimental/job/job_bg/cancel", json!({})).await.status.as_u16(), 204);
    assert_eq!(h.post("/api/experimental/job/job_missing/cancel", json!({})).await.status.as_u16(), 204);

    // A foreground job is listed once it moves to the background, and again when it ends.
    let (release, released) = tokio::sync::oneshot::channel::<()>();
    start("job_fg", false, Box::pin(async move {
        let _ = released.await;
        Ok("done".to_string())
    }));
    h.app.jobs.background("job_fg");
    subset(&next_update(&mut rx).await, json!({"id": "job_fg", "status": "running"}));
    release.send(()).unwrap();
    subset(&next_update(&mut rx).await, json!({"id": "job_fg", "status": "completed"}));

    // A job without a recovery is not listed.
    h.app.jobs.start("job_plain".into(), "shell".into(), None, None, Box::pin(async { Ok(String::new()) }));
    h.app.jobs.wait("job_plain").await;
    start("job_last", false, Box::pin(async { Ok(String::new()) }));
    subset(&next_update(&mut rx).await, json!({"id": "job_last", "status": "completed"}));
}
