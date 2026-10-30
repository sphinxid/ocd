//! Restart recovery (`core/session/execution/restart.ts`): resume root sessions whose execution
//! claim survived a process exit, and settle background jobs that were pending at that time.

use std::collections::HashSet;
use std::sync::Arc;

use parking_lot::Mutex;
use rusqlite::{params, OptionalExtension};
use serde_json::{json, Value};

use oc_schema::event::SessionEvent;
use oc_schema::types::SessionError;
use oc_tools::host::{JobRecovery, JobStatus};

use crate::job::{deliver_subagent, parse_status};
use crate::runner::Engine;

pub const DEFAULT_MAX_ATTEMPTS: i64 = 10;
pub const RESTART_TEXT: &str =
    "The server restarted while you were working. Continue from where you left off without repeating completed work.";
const RESUME_EXHAUSTED: &str = "Execution was interrupted repeatedly and will not be resumed automatically.";

/// A persisted `job.background/*` record.
struct Background {
    id: String,
    notification_id: String,
    recovery: JobRecovery,
    status: JobStatus,
    output: Option<String>,
    error: Option<String>,
}

fn background(v: &Value) -> Option<Background> {
    Some(Background {
        id: v.get("id")?.as_str()?.to_string(),
        notification_id: v.get("notificationID")?.as_str()?.to_string(),
        recovery: serde_json::from_value(v.get("recovery")?.clone()).ok()?,
        status: parse_status(v.get("status").and_then(Value::as_str).unwrap_or("cancelled")),
        output: v.get("output").and_then(Value::as_str).map(str::to_string),
        error: v.get("error").and_then(Value::as_str).map(str::to_string),
    })
}

impl Engine {
    /// `SessionStore.listSuspended`: root sessions with an unreleased claim.
    fn list_suspended(&self) -> anyhow::Result<Vec<String>> {
        self.app.db.with(|c| {
            let mut s = c.prepare("SELECT id FROM session_v2 WHERE time_suspended IS NOT NULL AND parent_id IS NULL")?;
            let r = s.query_map([], |r| r.get(0))?.collect();
            r
        })
    }

    /// `prepareResume`: count the attempt durably; terminalize past the budget, else admit the restart notice.
    async fn prepare_resume(&self, id: &str) -> anyhow::Result<bool> {
        let attempts: Option<i64> = self.app.db.with(|c| {
            c.query_row(
                "UPDATE session_v2 SET resume_attempts = resume_attempts + 1 WHERE id = ? RETURNING resume_attempts",
                [id],
                |r| r.get(0),
            )
            .optional()
        })?;
        let Some(attempts) = attempts else { return Ok(false) };
        if attempts > DEFAULT_MAX_ATTEMPTS {
            let session = id.to_string();
            self.app.bus.commit_with(
                vec![SessionEvent::ExecutionFailed { session_id: session.clone(), error: SessionError::new("aborted", RESUME_EXHAUSTED) }
                    .into()],
                |c, _, _| Ok(crate::session::Sessions::release(c, &session)?),
            )?;
            return Ok(false);
        }
        let meta = json!({"notice": "restart"}).as_object().cloned();
        let _ = self.app.sessions.synthetic(id, None, RESTART_TEXT.into(), Some("Continuing after restart".into()), meta, None).await;
        Ok(true)
    }

    async fn recover_shell(&self, bg: &Background) {
        let JobRecovery::Shell { session_id, shell_id, command } = &bg.recovery else { return };
        let state = if bg.status == JobStatus::Running { "cancelled" } else { bg.status.as_str() };
        let text = match (bg.status, state) {
            (JobStatus::Running, _) => "Command cancelled because the server restarted".to_string(),
            (_, "completed") => bg.output.clone().unwrap_or_else(|| "Command completed".into()),
            (_, "error") => bg.error.clone().unwrap_or_else(|| "Command failed".into()),
            _ => "Command cancelled".into(),
        };
        let notice = oc_tools::shell::result::notification(shell_id, Some(&bg.id), command, state, &text, None);
        // Restart notices must not revive idle owners of long-lived shells.
        let _ = self
            .app
            .sessions
            .synthetic(session_id, Some(bg.notification_id.clone()), notice.text, Some(command.clone()), Some(notice.metadata), None)
            .await;
        self.app.jobs.complete_background(&bg.notification_id);
    }

    /// `recoverImage`: an image job pending at the restart becomes its `<image>` notice.
    async fn recover_image(&self, bg: &Background) {
        let JobRecovery::Image { session_id, path, model } = &bg.recovery else { return };
        let body = match bg.status {
            JobStatus::Running => format!("Image request for {path} was cancelled because the server restarted."),
            JobStatus::Completed => bg.output.clone().unwrap_or_else(|| format!("Image request finished. Saved image: {path} ({model}).")),
            JobStatus::Error => bg.error.clone().unwrap_or_else(|| format!("Image request failed for {path}.")),
            _ => format!("Image request for {path} was cancelled."),
        };
        let state = if bg.status == JobStatus::Running { "cancelled" } else { bg.status.as_str() };
        let text = format!("<image id=\"{}\" state=\"{state}\">\n{body}\n</image>", bg.id);
        let metadata = json!({ "source": "experimental_image_generate", "jobID": bg.id, "state": state, "path": path });
        let _ = self
            .app
            .sessions
            .synthetic(session_id, Some(bg.notification_id.clone()), text, Some("Image generation".into()), metadata.as_object().cloned(), None)
            .await;
        self.app.jobs.complete_background(&bg.notification_id);
    }

    async fn recover_subagent(self: &Arc<Self>, bg: Background, suspended: &Arc<Mutex<HashSet<String>>>) {
        let JobRecovery::Subagent { parent_session_id, child_session_id, description, .. } = bg.recovery.clone() else { return };
        let parent_of_child: Option<Option<String>> = self
            .app
            .db
            .with(|c| c.query_row("SELECT parent_id FROM session_v2 WHERE id = ?", [&child_session_id], |r| r.get(0)).optional())
            .ok()
            .flatten();
        let parent_exists = self.app.sessions.get(&parent_session_id).is_ok();
        if parent_of_child.flatten().as_deref() != Some(parent_session_id.as_str()) || !parent_exists {
            self.app.jobs.complete_background(&bg.notification_id);
            return;
        }
        if bg.status != JobStatus::Running {
            let resume = !suspended.lock().contains(&parent_session_id);
            deliver_subagent(&self.app, bg.status, bg.output, bg.error, Some(bg.notification_id), &bg.recovery, resume).await;
            return;
        }
        let coordinator = self.app.coordinator();
        if coordinator.is_active(&child_session_id) {
            return;
        }
        if !self.prepare_resume(&child_session_id).await.unwrap_or(false) {
            let resume = !suspended.lock().contains(&parent_session_id);
            deliver_subagent(&self.app, JobStatus::Error, None, Some(RESUME_EXHAUSTED.into()), Some(bg.notification_id), &bg.recovery, resume)
                .await;
            return;
        }
        let app = self.app.clone();
        let child = child_session_id.clone();
        let run = Box::pin(async move {
            if let crate::coordinator::Exit::Failure(e) = app.coordinator().run(&child).await {
                return Err(e.to_string());
            }
            let msgs = app.db.session_messages(&child).map_err(|e| e.to_string())?;
            let last = msgs.iter().rev().map(|(_, m)| m).find(|m| {
                matches!(m, oc_schema::Message::Assistant(a) if a.time.completed.is_some() && a.error.is_none())
            });
            Ok(crate::job::subagent_text(last))
        });
        self.app.jobs.start(bg.id.clone(), "subagent".into(), Some(description), Some(bg.recovery.clone()), run);
        self.app.jobs.set_notification_id(&bg.id, &bg.notification_id);
        self.app.jobs.background(&bg.id);
        let (app, suspended, recovery, id) = (self.app.clone(), suspended.clone(), bg.recovery, bg.id);
        tokio::spawn(async move {
            let Some(info) = app.jobs.wait(&id).await else { return };
            // Async observers consult the suspended set at delivery; later completions wake parents normally.
            let resume = !suspended.lock().contains(&parent_session_id);
            deliver_subagent(&app, info.status, info.output, info.error, info.notification_id, &recovery, resume).await;
        });
    }

    /// `resumeSuspendedSessions` (called at managed server boot).
    pub async fn resume_suspended(self: &Arc<Self>) -> anyhow::Result<usize> {
        let coordinator = self.app.coordinator();
        let mut pending: Vec<Background> = self.app.jobs.pending_background().iter().filter_map(background).collect();
        let children: Vec<String> = pending
            .iter()
            .filter_map(|b| match &b.recovery {
                JobRecovery::Subagent { child_session_id, .. } if b.status == JobStatus::Running => Some(child_session_id.clone()),
                _ => None,
            })
            .collect();
        // Early notices wait for recovery's accounting, including sessions that exhaust their budget.
        let suspended: Arc<Mutex<HashSet<String>>> = Arc::new(Mutex::new(
            self.list_suspended()?.into_iter().chain(children.iter().cloned()).filter(|s| !coordinator.is_active(s)).collect(),
        ));
        self.release_child_claims(&children)?;
        // Admit shell outcomes before a recovered child can start its first model request.
        pending.sort_by_key(|b| matches!(b.recovery, JobRecovery::Subagent { .. }));
        for bg in pending {
            if self.app.jobs.get(&bg.id).is_some_and(|j| j.status == JobStatus::Running) {
                continue;
            }
            match bg.recovery {
                JobRecovery::Shell { .. } => self.recover_shell(&bg).await,
                JobRecovery::Image { .. } => self.recover_image(&bg).await,
                JobRecovery::Subagent { .. } => self.recover_subagent(bg, &suspended).await,
            }
        }
        // Background completion can wake a parent, so inspect local ownership only after recovery.
        let mut resumed = 0;
        for id in self.list_suspended()? {
            if coordinator.is_active(&id) {
                continue;
            }
            if !self.prepare_resume(&id).await? {
                continue;
            }
            let c = coordinator.clone();
            tokio::spawn(async move {
                c.run(&id).await;
            });
            resumed += 1;
        }
        suspended.lock().clear();
        Ok(resumed)
    }

    /// `SessionStore.releaseChildClaims`: clear orphaned child claims except recoverable subagent children.
    fn release_child_claims(&self, recoverable: &[String]) -> anyhow::Result<()> {
        self.app.db.with(|c| {
            let ids: Vec<String> = {
                let mut s = c.prepare("SELECT id FROM session_v2 WHERE time_suspended IS NOT NULL AND parent_id IS NOT NULL")?;
                let r: rusqlite::Result<Vec<String>> = s.query_map([], |r| r.get(0))?.collect();
                r?
            };
            for id in ids.iter().filter(|id| !recoverable.contains(id)) {
                c.execute("UPDATE session_v2 SET time_suspended = NULL, resume_attempts = 0 WHERE id = ?", params![id])?;
            }
            Ok(())
        })
    }
}
