//! Background slow Code Mode scripts (`core/src/codemode/job.ts`, 2.3.1 #64343, behind `full_codemode`).
//!
//! A session's `execute` call runs as a `codemode` job. Still running after [`FOREGROUND`], or moved by a
//! session-wide background request, it leaves the step with a receipt, and its result arrives later in the session
//! as a `<codemode>` synthetic message. Nested waits inside the script (a shell command's job) don't hold the
//! session (`Job.BlockingParent`), and the script's progress stops once it detaches.

use std::sync::Arc;

use futures::future::BoxFuture;
use oc_schema::{Metadata, ToolContent};
use oc_tools::host::{JobBlockResult, JobRecovery, JobStatus};
use oc_tools::tool::ToolErrorKind;
use oc_tools::{ToolContext, ToolError, ToolResult};
use serde_json::{json, Value};
use tokio_util::sync::CancellationToken;

use crate::app::App;

pub const KIND: &str = "codemode";
pub const TITLE: &str = "Code Mode script";
/// How long a session's `execute` call waits for its script before moving it to the background.
pub const FOREGROUND: std::time::Duration = std::time::Duration::from_secs(10);

/// A new script's job ID (`cm_<ascending>`).
pub fn id() -> String {
    oc_schema::id::ascending("cm")
}

/// The `<background-result kind="codemode">` notice of a settled (or, after a restart, interrupted) script: `(description, text, metadata)`.
pub fn notice(id: &str, status: JobStatus, output: Option<&str>, error: Option<&str>) -> (String, String, Metadata) {
    let state = if status == JobStatus::Running { "cancelled" } else { status.as_str() };
    let body = if status == JobStatus::Running { "Interrupted by server restart." } else { output.or(error).unwrap_or(state) };
    let mut metadata = Metadata::new();
    metadata.insert("source".into(), json!("codemode"));
    metadata.insert("jobID".into(), json!(id));
    metadata.insert("state".into(), json!(state));
    // The notice's description is the short "Script" (2.4.1, #64741); the job keeps its longer title.
    ("Script".into(), oc_tools::background_result::render(&format!("kind=\"codemode\" id=\"{id}\" state=\"{state}\""), body), metadata)
}

/// The receipt a backgrounded script's call returns.
pub fn receipt(id: &str) -> ToolResult {
    let text = format!("Script {id} is still running; its result will arrive in this session.");
    let mut metadata = Metadata::new();
    metadata.insert("jobID".into(), json!(id));
    ToolResult {
        content: vec![ToolContent::Text { text: text.clone() }],
        output: Some(json!({ "output": text, "toolCalls": [], "files": [] })),
        metadata: Some(metadata),
    }
}

/// The location's output limits and the data directory, for the job's text (`ToolOutput.truncate`/`save`).
#[derive(Clone)]
pub struct Output {
    pub max_lines: usize,
    pub max_bytes: usize,
    pub data: std::path::PathBuf,
}

impl Output {
    fn truncate(&self, text: String) -> String {
        let bounded = oc_tools::tool_output::truncate(ToolResult::text(text), self.max_lines, self.max_bytes, &self.data);
        bounded
            .content
            .iter()
            .filter_map(|part| match part {
                ToolContent::Text { text } => Some(text.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n")
    }
}

fn text_of(result: &ToolResult) -> String {
    result
        .content
        .iter()
        .filter_map(|part| match part {
            ToolContent::Text { text } => Some(text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

type Exit = Result<ToolResult, ToolError>;
pub type Execute = Box<dyn FnOnce(ToolContext) -> BoxFuture<'static, Exit> + Send>;

/// Whether the call's progress may still reach its row: cleared once the script detaches.
#[derive(Default)]
struct Attached {
    detached: parking_lot::Mutex<bool>,
}

/// Cancels the script's job when the call is dropped while it still waits for it (`onInterrupt` → `jobs.cancel`).
struct CancelOnDrop {
    app: Arc<App>,
    id: String,
    armed: bool,
}

impl Drop for CancelOnDrop {
    fn drop(&mut self) {
        if self.armed {
            self.app.jobs.cancel(&self.id);
        }
    }
}

/// `CodeModeJob.execute`: runs a session's `execute` call as the job `id`. `ctx.host` is the host the script's
/// tools use, whose waits name `id` as their blocking parent.
pub async fn run(app: Arc<App>, id: String, ctx: ToolContext, output: Output, execute: Execute) -> Exit {
    let session_id = ctx.session_id.clone();
    let (result_tx, mut result_rx) = tokio::sync::watch::channel::<Option<Exit>>(None);
    let attached = Arc::new(Attached::default());
    let job_ctx = {
        let mut job_ctx = ctx.clone();
        let progress = ctx.progress.clone();
        let attached = attached.clone();
        job_ctx.progress = Arc::new(move |mut update: Metadata| {
            let detached = attached.detached.lock();
            if !*detached {
                // The background flag tells clients this script can move to the background (2.4.1).
                update.insert("background".into(), Value::Bool(true));
                progress(update);
            }
        });
        // The script belongs to its job from here: the call's interruption reaches it only by cancelling the job.
        job_ctx.cancel = CancellationToken::new();
        job_ctx.stop = oc_tools::Stop::none();
        job_ctx
    };
    let run_output = output.clone();
    let run = Box::pin(async move {
        let cancel = job_ctx.cancel.clone();
        let _interrupt = cancel.clone().drop_guard();
        let exit = execute(job_ctx).await;
        let _ = result_tx.send(Some(exit.clone()));
        match exit {
            Err(error) if error.kind == ToolErrorKind::Interrupted => Err(error.message),
            Err(error) => Err(run_output.truncate(error.model_message())),
            Ok(result) => {
                let body = run_output.truncate(text_of(&result));
                let failed = result.metadata.as_ref().and_then(|m| m.get("error")) == Some(&Value::Bool(true));
                if failed { Err(body) } else { Ok(body) }
            }
        }
    });
    let mut started = Metadata::new();
    started.insert("background".into(), Value::Bool(true));
    ctx.progress(started);
    app.jobs.start_input(crate::job::StartInput {
        id: id.clone(),
        kind: KIND.into(),
        title: Some(TITLE.into()),
        metadata_session: None,
        recovery: Some(JobRecovery::Codemode { session_id: session_id.clone(), job_id: id.clone() }),
        notification_id: None,
        background: false,
        run,
    });
    let mut cancel_job = CancelOnDrop { app: app.clone(), id: id.clone(), armed: true };
    let blocked = tokio::select! {
        blocked = tokio::time::timeout(FOREGROUND, app.jobs.block(&id, &session_id)) => blocked.ok().flatten(),
        _ = ctx.cancel.cancelled() => {
            // Dropping the guard cancels the job.
            return Err(ToolError::interrupted());
        }
    };
    if let Some(JobBlockResult::Finished(_)) = &blocked {
        cancel_job.armed = false;
        // A job cancelled before its script settled has no result: the call ends interrupted.
        let settled = result_rx.wait_for(Option::is_some).await.ok().and_then(|exit| exit.clone());
        return settled.unwrap_or_else(|| Err(ToolError::interrupted()));
    }
    let job = app.jobs.background(&id);
    cancel_job.armed = false;
    let Some(notification_id) = job.and_then(|job| job.notification_id) else { return Err(ToolError::interrupted()) };
    // The last progress update is out before the tool row becomes terminal.
    *attached.detached.lock() = true;
    tokio::spawn(report(app, id.clone(), session_id, notification_id, result_rx, output));
    Ok(receipt(&id))
}

/// Admits the script's result in its session once its job settles, and acknowledges the job's row.
async fn report(
    app: Arc<App>,
    id: String,
    session_id: String,
    notification_id: String,
    mut result: tokio::sync::watch::Receiver<Option<Exit>>,
    output: Output,
) {
    let Some(info) = app.jobs.wait(&id).await else { return };
    // Session removal cancels the job and removes its outbox row.
    if info.status == JobStatus::Cancelled {
        return;
    }
    // Synthetic messages carry text: the script's file results are saved instead of put in its inbox as base64.
    let exit = result.wait_for(Option::is_some).await.ok().and_then(|exit| exit.clone());
    let files: Vec<(String, String)> = match &exit {
        Some(Ok(result)) => result
            .content
            .iter()
            .filter_map(|part| match part {
                ToolContent::File { uri, mime, .. } => Some((uri.clone(), mime.clone())),
                _ => None,
            })
            .collect(),
        _ => Vec::new(),
    };
    let paths: Vec<String> = files.iter().map(|(uri, mime)| save(&output, uri, mime)).collect();
    let message = if paths.is_empty() {
        notice(&id, info.status, info.output.as_deref(), info.error.as_deref())
    } else {
        let mut lines: Vec<String> = info.output.clone().or(info.error.clone()).into_iter().collect();
        lines.extend(paths);
        let body = output.truncate(lines.join("\n"));
        notice(&id, info.status, Some(&body), None)
    };
    deliver(&app, &session_id, &notification_id, message).await;
}

/// `output.save` of a file result: a data URI's bytes under the tool-output directory, any other URI as is.
fn save(output: &Output, uri: &str, mime: &str) -> String {
    let prefix = format!("data:{mime};base64,");
    let Some(data) = uri.strip_prefix(&prefix) else { return uri.to_string() };
    let extension = oc_tools::mime::extension(mime).map(|e| format!(".{e}")).unwrap_or_default();
    use base64::Engine as _;
    let saved = base64::engine::general_purpose::STANDARD
        .decode(data)
        .ok()
        .and_then(|bytes| oc_tools::tool_output::save(&output.data, &bytes, &extension).ok());
    match saved {
        Some(path) => path.to_string_lossy().into_owned(),
        None => format!("Could not save {mime} attachment."),
    }
}

/// Admits the notice (from the call's own context, so at the session's location) and wakes the session.
async fn deliver(app: &Arc<App>, session_id: &str, notification_id: &str, (description, text, metadata): (String, String, Metadata)) {
    let admitted =
        app.sessions.synthetic(session_id, Some(notification_id.to_string()), text, Some(description), Some(metadata), None).await;
    match admitted {
        Ok(_) => {
            if let (Some(c), Ok(session)) = (app.coordinator.get(), app.sessions.get(session_id))
                && session.revert.is_none()
            {
                c.wake(session_id, crate::inbox::Scope::Input);
            }
        }
        Err(crate::session::SessionError::NotFound(_)) => {}
        Err(error) => {
            tracing::warn!(sessionID = session_id, error = %error, "failed to report a Code Mode script");
            return;
        }
    }
    app.jobs.complete_background(notification_id);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn notices() {
        let (description, text, metadata) = notice("cm_1", JobStatus::Completed, Some("result: done"), None);
        assert_eq!(description, "Script");
        assert_eq!(text, "<background-result kind=\"codemode\" id=\"cm_1\" state=\"completed\">\nresult: done\n</background-result>");
        assert_eq!(serde_json::to_value(metadata).unwrap(), json!({ "source": "codemode", "jobID": "cm_1", "state": "completed" }));
        // A failed script reports its error; one still running at a restart was interrupted by it.
        assert_eq!(notice("cm_2", JobStatus::Error, None, Some("boom")).1, "<background-result kind=\"codemode\" id=\"cm_2\" state=\"error\">\nboom\n</background-result>");
        assert_eq!(
            notice("cm_3", JobStatus::Running, Some("partial"), None).1,
            "<background-result kind=\"codemode\" id=\"cm_3\" state=\"cancelled\">\nInterrupted by server restart.\n</background-result>"
        );
        assert_eq!(notice("cm_4", JobStatus::Cancelled, None, None).1, "<background-result kind=\"codemode\" id=\"cm_4\" state=\"cancelled\">\ncancelled\n</background-result>");
        let receipt = receipt("cm_5");
        assert_eq!(receipt.output, Some(json!({ "output": "Script cm_5 is still running; its result will arrive in this session.", "toolCalls": [], "files": [] })));
        assert_eq!(serde_json::to_value(receipt.metadata).unwrap(), json!({ "jobID": "cm_5" }));
        assert!(id().starts_with("cm_"));
    }
}
