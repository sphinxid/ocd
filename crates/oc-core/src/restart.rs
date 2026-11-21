//! Restart recovery (`core/session/execution/restart.ts`, 2.3.0): resume sessions whose execution claim survived a
//! process exit, and settle background jobs that were pending at that time. Shell and image outcomes are admitted
//! first, then subagent rows: a finished row replays its formatted report, a running child only counts its restart
//! attempt. Wakes from replayed reports wait until every suspended session counted its attempt; directly driven
//! children resume like root sessions; recovered subagent jobs start last.
//!
//! Shell commands that survived the restart in the process daemon are taken back: a running shell job is watched
//! again (`ShellTool.reattach`), and a foreground shell call the previous server left unsettled is settled as moved
//! to the background (`settleBackgroundedCalls`). A shell job whose command is gone is reported cancelled.

use std::collections::HashSet;
use std::sync::Arc;

use parking_lot::Mutex;
use rusqlite::OptionalExtension;
use serde_json::{json, Value};

use oc_schema::event::SessionEvent;
use oc_schema::types::SessionError;
use oc_tools::host::{JobRecovery, JobStatus};

use crate::job::{parse_status, subagent_notice, SubagentNotice};
use crate::session::SessionError as Admission;
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
    /// `SessionStore.listSuspended`: sessions with an unreleased claim, including directly driven children.
    fn list_suspended(&self) -> anyhow::Result<Vec<String>> {
        self.app.db.with(|c| {
            let mut s = c.prepare("SELECT id FROM session_v2 WHERE time_suspended IS NOT NULL")?;
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

    /// `shellOf(sessionID)`: the shell service once the session's location took back the commands the daemon still
    /// runs for it, with the location's output limits; `None` for a session that is gone, a remote workspace or a
    /// directory that is gone (whose commands are reported like any lost background work).
    async fn shell_of(&self, session_id: &str) -> Option<(usize, usize)> {
        let session = self.app.sessions.get(session_id).ok()?;
        if session.location.workspace_id.is_some() || !std::path::Path::new(&session.location.directory).is_dir() {
            return None;
        }
        // Commands are kept under the location's canonical directory, as the shell tool started them.
        let location = self.location(&session.location.directory).await.ok()?;
        self.shell.recover(&location.directory.to_string_lossy(), None, &session.project_id).await;
        let limits = oc_config::fold::tool_output(&location.entries).map(|t| (t.max_lines, t.max_bytes));
        let (lines, bytes) = limits.unwrap_or_default();
        Some((
            lines.map(|v| v as usize).unwrap_or(oc_tools::tool_output::MAX_LINES),
            bytes.map(|v| v as usize).unwrap_or(oc_tools::tool_output::MAX_BYTES),
        ))
    }

    /// `ShellTool.reattach`: watches a command taken over from a previous server as a background job of its session,
    /// and notifies the session when it finishes, as for a command the model ran in the background. A finished
    /// command's notice is admitted before this returns.
    async fn reattach_shell(
        self: &Arc<Self>,
        notices: &Notices,
        session_id: &str,
        info: oc_schema::message::ShellInfo,
        notification_id: Option<String>,
        limits: (usize, usize),
    ) {
        let job = oc_tools::tools::shell::job(self.shell.clone(), info.clone(), limits.0, limits.1, None);
        let started = self.app.jobs.start_input(crate::job::StartInput {
            id: info.id.clone(),
            kind: "shell".into(),
            title: Some(info.command.clone()),
            metadata_session: Some(session_id.to_string()),
            recovery: Some(JobRecovery::Shell { session_id: session_id.into(), shell_id: info.id.clone(), command: info.command.clone() }),
            notification_id,
            background: true,
            run: job.run,
        });
        let finished = self.shell.get(&info.id).is_ok_and(|current| current.status != oc_schema::message::ShellStatus::Running);
        let notify = {
            let app = self.app.clone();
            let mut settled = job.settled;
            let (job_id, shell_id, command, session) = (started.id.clone(), info.id.clone(), info.command.clone(), session_id.to_string());
            async move {
                let job = app.jobs.wait(&job_id).await?;
                if job.status == JobStatus::Running {
                    return None;
                }
                let output = if job.status == JobStatus::Completed {
                    settled.wait_for(Option::is_some).await.ok().and_then(|o| o.clone())
                } else {
                    None
                };
                let note = oc_tools::tools::shell::job_notice(&shell_id, &job, &command, output.as_ref());
                Some((SubagentNotice { id: job.notification_id.clone(), session_id: session, text: note.text, description: command, metadata: note.metadata }, job.notification_id))
            }
        };
        if finished {
            if let Some((notice, notification)) = notify.await {
                // A session deleted while its command ran has no one left to tell.
                match notices.synthetic(&self.app, notice).await {
                    Ok(()) | Err(Admission::NotFound(_)) => {
                        if let Some(id) = notification {
                            self.app.jobs.complete_background(&id);
                        }
                    }
                    Err(error) => tracing::warn!(id = %info.id, error = %error, "failed to report a recovered shell command"),
                }
            }
            return;
        }
        let engine = self.clone();
        tokio::spawn(async move {
            let Some((notice, notification)) = notify.await else { return };
            let session = notice.session_id.clone();
            let admitted = engine
                .app
                .sessions
                .synthetic(&session, notice.id, notice.text, Some(notice.description), Some(notice.metadata), None)
                .await;
            match admitted {
                Ok(_) => engine.wake_owner(&session),
                Err(Admission::NotFound(_)) => {}
                Err(error) => {
                    tracing::warn!(sessionID = %session, error = %error, "failed to report a recovered shell command");
                    return;
                }
            }
            if let Some(id) = notification {
                engine.app.jobs.complete_background(&id);
            }
        });
    }

    /// `recoverShellJob`: a shell job the previous server left running is watched again if its command is still
    /// known (running, or finished during the restart); otherwise it is reported as lost.
    async fn recover_shell_job(self: &Arc<Self>, bg: &Background, notices: &Notices, reattached: &mut HashSet<String>) {
        let JobRecovery::Shell { session_id, shell_id, .. } = &bg.recovery else { return };
        if bg.status == JobStatus::Running
            && let Some(limits) = self.shell_of(session_id).await
            && let Ok(info) = self.shell.get(shell_id)
        {
            reattached.insert(info.id.clone());
            self.reattach_shell(notices, session_id, info, Some(bg.notification_id.clone()), limits).await;
            return;
        }
        self.recover_shell(bg).await;
    }

    /// `settleBackgroundedCalls`: a server that stopped (shut down or killed) during a foreground shell call leaves
    /// the call unsettled, its command still running in the daemon. Settle the call as moved to the background and
    /// watch the command.
    async fn settle_backgrounded_calls(self: &Arc<Self>, session_id: &str, notices: &Notices, reattached: &HashSet<String>) -> anyhow::Result<()> {
        let messages = self.app.db.session_messages(session_id)?;
        let since = messages
            .iter()
            .filter(|(_, m)| matches!(m, oc_schema::Message::Compaction(c) if matches!(c.status, oc_schema::message::CompactionStatus::Completed { .. })))
            .map(|(seq, _)| *seq)
            .max();
        let calls: Vec<(String, String, bool)> = messages
            .iter()
            .filter(|(seq, _)| since.is_none_or(|since| *seq >= since))
            .filter_map(|(_, m)| match m {
                oc_schema::Message::Assistant(a) if a.time.completed.is_none() => Some(a),
                _ => None,
            })
            .flat_map(|a| {
                a.content.iter().filter_map(|part| match part {
                    oc_schema::message::AssistantContent::Tool { id, name, executed, state: oc_schema::message::ToolState::Running { .. }, .. }
                        if name == "shell" =>
                    {
                        Some((a.id.clone(), id.clone(), executed == &Some(true)))
                    }
                    _ => None,
                })
            })
            .collect();
        if calls.is_empty() {
            return Ok(());
        }
        let Some(limits) = self.shell_of(session_id).await else { return Ok(()) };
        for (assistant, call, executed) in calls {
            let tool_call = oc_tools::shell::daemon::ToolCall {
                session_id: session_id.into(),
                assistant_message_id: assistant.clone(),
                call_id: call.clone(),
            };
            let Some(info) = self.shell.by_tool_call(&tool_call) else { continue };
            let (content, metadata) = oc_tools::tools::shell::background_tool_result(&info);
            self.app.bus.commit_one(SessionEvent::ToolSuccess {
                session_id: session_id.into(),
                assistant_message_id: assistant,
                id: call,
                content,
                metadata: Some(metadata),
                executed,
                native: None,
            })?;
            // Backgrounded but stopped before the call was settled: its job row was already reattached (and, for a
            // command that already finished, may even be done).
            if reattached.contains(&info.id) {
                continue;
            }
            self.reattach_shell(notices, session_id, info, None, limits).await;
        }
        Ok(())
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

    /// `recoverToolJob` for a Code Mode script (`CodeModeJob.notice`): one still running was interrupted by the
    /// restart; a settled one replays its result.
    async fn recover_codemode(&self, bg: &Background) {
        let JobRecovery::Codemode { session_id, .. } = &bg.recovery else { return };
        let (description, text, metadata) = crate::codemode_job::notice(&bg.id, bg.status, bg.output.as_deref(), bg.error.as_deref());
        // Restart notices must not revive idle owners of background tools.
        let _ = self
            .app
            .sessions
            .synthetic(session_id, Some(bg.notification_id.clone()), text, Some(description), Some(metadata), None)
            .await;
        self.app.jobs.complete_background(&bg.notification_id);
    }

    /// `deliverSubagentSafe`: replay a formatted report. A deleted parent acknowledges the row; a conflicting
    /// notification ID keeps it for the next boot; any other failure is logged and keeps it too.
    async fn deliver_subagent_safe(&self, notices: &Notices, bg_nid: &str, recovery: &JobRecovery, status: JobStatus, output: Option<String>, error: Option<String>) {
        let JobRecovery::Subagent { parent_session_id, child_session_id, .. } = recovery else { return };
        let Some(notice) = subagent_notice(status, output, error, Some(bg_nid.to_string()), recovery) else { return };
        match notices.synthetic(&self.app, notice).await {
            Ok(()) => self.app.jobs.complete_background(bg_nid),
            Err(Admission::NotFound(_)) => self.app.jobs.complete_background(bg_nid),
            Err(Admission::PromptConflict(_)) => tracing::warn!(
                notificationID = bg_nid,
                parentSessionID = parent_session_id.as_str(),
                childSessionID = child_session_id.as_str(),
                "Failed to admit recovered subagent notification due to ID conflict"
            ),
            Err(e) => {
                if self.app.sessions.get(parent_session_id).is_err() {
                    self.app.jobs.complete_background(bg_nid);
                } else {
                    tracing::warn!(
                        notificationID = bg_nid,
                        parentSessionID = parent_session_id.as_str(),
                        childSessionID = child_session_id.as_str(),
                        error = %e,
                        "Unexpected failure delivering recovered subagent notification"
                    );
                }
            }
        }
    }

    /// `recoverSubagent`: replay a finished row, terminalize a child past its budget, or count its attempt and report
    /// it as resumable (it starts only after every suspended session counted its own).
    async fn recover_subagent(&self, bg: &Background, notices: &Notices) -> bool {
        let JobRecovery::Subagent { parent_session_id, child_session_id, .. } = &bg.recovery else { return false };
        let parent_of_child: Option<Option<String>> = self
            .app
            .db
            .with(|c| c.query_row("SELECT parent_id FROM session_v2 WHERE id = ?", [child_session_id], |r| r.get(0)).optional())
            .ok()
            .flatten();
        let parent_exists = self.app.sessions.get(parent_session_id).is_ok();
        if parent_of_child.flatten().as_deref() != Some(parent_session_id.as_str()) || !parent_exists {
            self.app.jobs.complete_background(&bg.notification_id);
            return false;
        }
        if bg.status != JobStatus::Running {
            // The outbox row already holds the formatted result; replay it without touching the child.
            self.deliver_subagent_safe(notices, &bg.notification_id, &bg.recovery, bg.status, bg.output.clone(), bg.error.clone()).await;
            return false;
        }
        if self.app.coordinator().is_active(child_session_id) {
            return false;
        }
        // An exhausted child is terminalized directly on the bus, outside the coordinator, so no report settles it.
        if !self.prepare_resume(child_session_id).await.unwrap_or(false) {
            self.deliver_subagent_safe(notices, &bg.notification_id, &bg.recovery, JobStatus::Error, None, Some(RESUME_EXHAUSTED.into()))
                .await;
            return false;
        }
        true
    }

    /// `resumeSuspendedSessions` (called at managed server boot). Returns how many sessions it resumed.
    pub async fn resume_suspended(self: &Arc<Self>) -> anyhow::Result<usize> {
        let coordinator = self.app.coordinator();
        let active: HashSet<String> = coordinator.active().into_iter().collect();
        let pending: Vec<Background> = self.app.jobs.pending_background().iter().filter_map(background).collect();
        let children: HashSet<String> = pending
            .iter()
            .filter_map(|b| match &b.recovery {
                JobRecovery::Subagent { child_session_id, .. } if b.status == JobStatus::Running => Some(child_session_id.clone()),
                _ => None,
            })
            .collect();
        self.release_orphaned_child_claims()?;
        // A cancel persisted before its child could release the claim (a crash in between) leaves the claim behind.
        for bg in &pending {
            if let (JobStatus::Cancelled, JobRecovery::Subagent { child_session_id, .. }, None) = (bg.status, &bg.recovery, &bg.error) {
                if !active.contains(child_session_id) && !children.contains(child_session_id) {
                    self.app.db.with(|c| crate::session::Sessions::release(c, child_session_id))?;
                }
            }
        }
        // Early notices wait for recovery's accounting, including sessions that exhaust their budget.
        let suspended: HashSet<String> =
            self.list_suspended()?.into_iter().chain(children.iter().cloned()).filter(|s| !active.contains(s)).collect();
        let notices = Notices { suspended: Mutex::new(suspended), wakes: Mutex::new(Vec::new()) };
        let result = self.sweep(&pending, &children, &notices).await;
        // Deferred wakes go out even when the sweep failed part way.
        let wakes = std::mem::take(&mut *notices.wakes.lock());
        for id in wakes {
            self.wake_owner(&id);
        }
        result
    }

    async fn sweep(self: &Arc<Self>, pending: &[Background], children: &HashSet<String>, notices: &Notices) -> anyhow::Result<usize> {
        let coordinator = self.app.coordinator();
        let recoverable = |bg: &Background| !self.app.jobs.get(&bg.id).is_some_and(|j| j.status == JobStatus::Running);
        // Shell and tool outcomes first, then calls a killed server left unsettled, and only then recovered children,
        // whose first resume would otherwise fail their own unsettled shell calls.
        let mut reattached = HashSet::new();
        for bg in pending.iter().filter(|b| !matches!(b.recovery, JobRecovery::Subagent { .. })) {
            if !recoverable(bg) {
                continue;
            }
            match bg.recovery {
                JobRecovery::Shell { .. } => self.recover_shell_job(bg, notices, &mut reattached).await,
                JobRecovery::Image { .. } => self.recover_image(bg).await,
                JobRecovery::Codemode { .. } => self.recover_codemode(bg).await,
                JobRecovery::Subagent { .. } => {}
            }
        }
        let suspended: Vec<String> = notices.suspended.lock().iter().cloned().collect();
        for session in suspended {
            if let Err(error) = self.settle_backgrounded_calls(&session, notices, &reattached).await {
                tracing::warn!(sessionID = %session, error = %error, "failed to settle backgrounded shell calls");
            }
        }
        let mut resumable = Vec::new();
        for bg in pending.iter().filter(|b| matches!(b.recovery, JobRecovery::Subagent { .. })) {
            if recoverable(bg) && self.recover_subagent(bg, notices).await {
                resumable.push(bg);
            }
        }
        // A directly driven child can also settle and wake its parent. Count every claimed execution before starting
        // any of them, leaving running jobs to their subagent job.
        let resumed: HashSet<String> = coordinator.active().into_iter().collect();
        let mut direct = Vec::new();
        for id in self.list_suspended()? {
            if resumed.contains(&id) || children.contains(&id) {
                continue;
            }
            if self.prepare_resume(&id).await? {
                direct.push(id);
            }
        }
        let count = direct.len() + resumable.len();
        for id in direct {
            notices.suspended.lock().remove(&id);
            let c = coordinator.clone();
            tokio::spawn(async move {
                c.run(&id).await;
            });
        }
        // Start children only now: their reports wake parents, whose restart budget must already be charged.
        for bg in resumable {
            crate::job::start_subagent(&self.app, bg.recovery.clone(), Some(bg.notification_id.clone()));
            if let JobRecovery::Subagent { child_session_id, .. } = &bg.recovery {
                notices.suspended.lock().remove(child_session_id);
            }
        }
        Ok(count)
    }

    /// `wake`: wake a session that still exists and has no staged revert.
    fn wake_owner(&self, id: &str) {
        if let Ok(s) = self.app.sessions.get(id) {
            if s.revert.is_none() {
                self.app.coordinator().wake(id, crate::inbox::Scope::Input);
            }
        }
    }

    /// `SessionStore.releaseOrphanedChildClaims`: release the claims of children whose parent was deleted.
    fn release_orphaned_child_claims(&self) -> anyhow::Result<()> {
        self.app.db.with(|c| {
            c.execute(
                "UPDATE session_v2 SET time_suspended = NULL, resume_attempts = 0 \
                 WHERE time_suspended IS NOT NULL AND parent_id NOT IN (SELECT id FROM session_v2)",
                [],
            )
        })?;
        Ok(())
    }
}

/// Restart notices admitted without waking; a wake is held until every suspended session counted its restart
/// attempt (`notices.synthetic` in `resumeSuspendedSessions`), and never sent to a session the sweep resumes.
struct Notices {
    suspended: Mutex<HashSet<String>>,
    wakes: Mutex<Vec<String>>,
}

impl Notices {
    async fn synthetic(&self, app: &crate::app::App, notice: SubagentNotice) -> Result<(), Admission> {
        let session = notice.session_id.clone();
        app.sessions.synthetic(&session, notice.id, notice.text, Some(notice.description), Some(notice.metadata), None).await?;
        if !self.suspended.lock().contains(&session) {
            let mut wakes = self.wakes.lock();
            if !wakes.contains(&session) {
                wakes.push(session);
            }
        }
        Ok(())
    }
}
