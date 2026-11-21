//! `shell` (`core/src/tool/plugin/shell.ts`).

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use async_trait::async_trait;
use futures::FutureExt;
use oc_schema::ToolContent;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::file_access::{self, Kind};
use crate::host::{JobBlockResult, JobRecovery, JobStart, JobStatus, ShellInvocation, SyntheticMessage};
use crate::shell::result::{self as shell_result, Output};
use crate::shell::daemon::{Placement, ToolCall};
use crate::shell::service::ShellService;
use crate::shell::{parse, select};
use crate::tool::{DefinitionContext, ErrorCause, Tool, ToolContext, ToolDefinition, ToolError, ToolOptions, ToolResult, decode, meta};

pub const DEFAULT_TIMEOUT_MS: u64 = 2 * 60 * 1_000;
/// The default timeout of a `tools.shell` call in a Code Mode program (`full_codemode`, 2.3.1 #64335).
pub const CODE_MODE_TIMEOUT_MS: u64 = 10 * 60 * 1_000;
/// How long a call the user interrupted waits for its stopped command to exit before settling with the output so
/// far (`INTERRUPT_WAIT`).
pub const INTERRUPT_WAIT: std::time::Duration = std::time::Duration::from_millis(500);

pub const BACKGROUND_INSTRUCTION: &str = "You will be notified automatically when the command finishes. The notification will include the command's output. Unless the user explicitly asks otherwise, DO NOT poll for completion, even if you need the final result to continue. Repeatedly sleeping and reading or searching the output file is polling, not useful work. You may read the current output if it lets you do useful work now, but do not repeatedly check it while waiting for the command to finish. Keep working on anything that does not depend on the result. If you have nothing else to do, end your response; you will be resumed automatically when the command finishes.";

fn os_name() -> &'static str {
    match std::env::consts::OS {
        "macos" => "macOS",
        "windows" => "Windows",
        "linux" => "Linux",
        other => other,
    }
}

/// Tool description; `shell` is `ShellSelect.name(compatShell)` after the session hook.
pub fn description(shell: Option<&str>) -> String {
    description_for(shell, false)
}

/// [`description`] for the direct tool or, with `code_mode`, for `tools.shell` (`full_codemode`), which has no
/// background parameter.
pub fn description_for(shell: Option<&str>, code_mode: bool) -> String {
    let mut parts = vec!["Execute a shell command and return its output.".to_string()];
    if let Some(shell) = shell {
        parts.push(format!("Commands run on {} using {shell}.", os_name()));
    }
    parts.extend(
        [
            "Quote file paths containing spaces or special characters.",
            "Prefer dedicated tools over shell commands when possible.",
            "When output is large, the full result is saved to a file and a truncated preview is returned.",
            "Rely on automatic truncation unless filtering the output is more useful.",
            "Commands still running after their timeout move to the background.",
        ]
        .map(String::from),
    );
    // Code Mode has no background parameter.
    if !code_mode {
        parts.push("Background commands return immediately, and you will be notified when they complete.".into());
    }
    parts.push("The current session ID is available in the OPENCODE_SESSION_ID environment variable.".into());
    parts.join(" ")
}

/// The input and output schemas of `tools.shell` (`CodeModeInput`): no `background`, and the timeout's default is
/// [`CODE_MODE_TIMEOUT_MS`].
pub fn code_mode_definition() -> ToolDefinition {
    let mut definition = super::definition("shell", description_for(None, true), include_str!("../../schemas/shell.json"));
    if let Some(properties) = definition.input_schema.get_mut("properties").and_then(Value::as_object_mut) {
        properties.shift_remove("background");
        if let Some(timeout) = properties.get_mut("timeout") {
            timeout["description"] = json!(timeout_description(CODE_MODE_TIMEOUT_MS));
        }
    }
    definition
}

fn timeout_description(foreground: u64) -> String {
    format!(
        "Optional timeout in milliseconds (default {foreground}). When it expires, a foreground command moves to the background and a background command is stopped."
    )
}

/// A finished command's result as its call or job reports it (`Output`).
#[derive(Debug, Clone, PartialEq, Default)]
pub struct ShellOutput {
    pub base: Output,
    shell_id: Option<String>,
    running: bool,
    /// Seconds from the command's start to its end (or to now, for a command still running).
    wall_time_seconds: f64,
    /// The output file, when the output was cut or the command runs on in the background.
    output_path: Option<String>,
    pid: Option<u32>,
}

fn result_messages(output: &ShellOutput, timeout: Option<u64>) -> Vec<String> {
    let notice = if output.running { Some(BACKGROUND_INSTRUCTION.to_string()) } else { shell_result::notice(&output.base, timeout) };
    let mut out = Vec::new();
    if !output.base.output.is_empty() {
        out.push(output.base.output.clone());
    }
    if let Some(n) = notice {
        out.push(n);
    }
    out
}

/// `wallTimeSeconds`: elapsed seconds, never negative.
fn wall_time_seconds(started: u64, end: u64) -> f64 {
    end.saturating_sub(started) as f64 / 1_000.0
}

/// The structured output (`StructuredOutput`, 2.3.1), in its schema's key order.
fn structured(output: &ShellOutput) -> Value {
    let mut value = serde_json::Map::new();
    if let Some(id) = &output.shell_id {
        value.insert("shellID".into(), json!(id));
    }
    value.insert("exitCode".into(), output.base.exit.map_or(Value::Null, |exit| json!(exit)));
    if let Some(signal) = &output.base.signal {
        value.insert("signal".into(), json!(signal));
    }
    value.insert("output".into(), json!(output.base.output));
    if let Some(path) = &output.output_path {
        value.insert("outputPath".into(), json!(path));
    }
    if let Some(pid) = output.pid {
        value.insert("pid".into(), json!(pid));
    }
    // A JS number: whole seconds have no fraction.
    let seconds = output.wall_time_seconds;
    value.insert("wallTimeSeconds".into(), if seconds.fract() == 0.0 { json!(seconds as i64) } else { json!(seconds) });
    Value::Object(value)
}

fn tool_result(output: ShellOutput, timeout: Option<u64>) -> ToolResult {
    let mut metadata = meta(vec![("status", json!(if output.running { "running" } else { "completed" }))]);
    for (k, v) in shell_result::metadata(&output.base) {
        metadata.insert(k, v);
    }
    if let Some(id) = &output.shell_id {
        metadata.insert("shellID".into(), json!(id));
    }
    if let Some(pid) = output.pid {
        metadata.insert("pid".into(), json!(pid));
    }
    ToolResult {
        content: result_messages(&output, timeout).into_iter().map(|text| ToolContent::Text { text }).collect(),
        output: Some(structured(&output)),
        metadata: Some(metadata),
    }
}

/// `backgroundText`: what a call whose command moved to the background returns; `timeout` (ms) when its foreground
/// timeout moved it there.
fn background_text(info: &oc_schema::message::ShellInfo, timeout: Option<u64>) -> String {
    let mut lines = vec![format!("Command moved to the background (shell ID: {}).", info.id)];
    if let Some(pid) = info.pid {
        lines.push(format!("PID: {pid}"));
    }
    lines.push(format!("Output is streaming to: {}", info.file));
    if let Some(timeout) = timeout {
        lines.push(String::new());
        lines.push(format!(
            "The command did not finish within its timeout of {} seconds and was moved to the background. Do not start another copy while it is running.",
            crate::util::js::number(timeout as f64 / 1_000.0)
        ));
    }
    lines.join("\n")
}

fn background_result(info: &oc_schema::message::ShellInfo, now: u64, timeout: Option<u64>) -> ShellOutput {
    ShellOutput {
        base: Output { output: background_text(info, timeout), truncated: false, ..Default::default() },
        shell_id: Some(info.id.clone()),
        running: true,
        wall_time_seconds: wall_time_seconds(info.time.started, now),
        output_path: Some(info.file.clone()),
        pid: info.pid,
    }
}

/// Stops the command when the job's run is dropped before it settled: cancelling the job (or interrupting the call
/// that waits for it) stops the command but keeps it, output file included (`onInterrupt` → `shell.stop`, 2.3.0).
///
/// The call holds one too, with its job once started: upstream's call is uninterruptible from spawning the command
/// to blocking on its job, and an interrupt while it blocks cancels the job (`onInterrupt` → `jobs.cancel`). Here an
/// interrupted call is dropped wherever it waits, so the stop and the cancel come from the drop; without them the
/// job would keep waiting for the command, which would run on.
struct StopOnDrop {
    service: Arc<ShellService>,
    id: String,
    armed: bool,
    job: Option<(Arc<dyn crate::host::ToolHost>, String)>,
}

impl Drop for StopOnDrop {
    fn drop(&mut self) {
        // A server handing its daemon over leaves the command (and its job) running for the next server, which takes
        // the call over and moves the command to the background.
        if !self.armed || self.service.handoff_prepared() {
            return;
        }
        let _ = self.service.stop(&self.id, oc_schema::message::ShellStatus::Killed);
        if let (Some((host, job)), Ok(runtime)) = (self.job.take(), tokio::runtime::Handle::try_current()) {
            runtime.spawn(async move {
                let _ = host.job_cancel(&job).await;
            });
        }
    }
}

#[derive(Deserialize)]
struct Input {
    command: String,
    workdir: Option<String>,
    timeout: Option<u64>,
    background: Option<bool>,
}

pub struct ShellTool {
    service: Arc<ShellService>,
}

impl ShellTool {
    pub fn new(service: Arc<ShellService>) -> Self {
        Self { service }
    }

    /// Compatible shell for this context (`ShellSelect.resolve({priority:"compat"})`).
    pub fn compatible_shell(ctx: &ToolContext) -> String {
        select::resolve(select::Priority::Compat, ctx.config.shell.as_deref(), Some(&ctx.global.bin))
    }

    /// Stops the command of a call the user asked to stop, and fails the call with the output so far. The command
    /// stays observable, output file included (`cancelled`, 2.3.0).
    async fn cancelled(&self, ctx: &ToolContext, info: &oc_schema::message::ShellInfo, job_id: &str) -> ToolError {
        let _ = self.service.stop(&info.id, oc_schema::message::ShellStatus::Killed);
        // The stop escalates to SIGKILL on its own; the session does not wait for that to settle the call.
        let result = self.service.result_within(info, Some(INTERRUPT_WAIT), ctx.config.max_lines(), ctx.config.max_bytes()).await;
        // The stopped job settles from the same exit; this only releases it.
        let _ = ctx.host.job_cancel(job_id).await;
        let stopped = shell_result::interrupted(&result);
        // The notice ends the message; renderers show what precedes it as the output, like a finished call's.
        ToolError::new(stopped.message).with_metadata(meta(vec![
            ("interrupted", json!(true)),
            ("truncated", json!(result.capture.as_ref().is_some_and(|c| c.truncated))),
            ("shellID", json!(info.id)),
            ("notice", json!(stopped.notice)),
        ]))
    }

    async fn prepare(&self, invocation: &mut ShellInvocation, ctx: &ToolContext, leaf: &str) -> Result<u64, ToolError> {
        let target = file_access::resolve(ctx, &invocation.cwd, Some(Kind::Directory));
        invocation.cwd = target.absolute.to_string_lossy().into_owned();
        let timeout = invocation.timeout;
        let parsed = parse::scan(&invocation.command, &invocation.shell, &invocation.cwd, &ctx.global.home)
            .map_err(|message| ToolError::with_cause(leaf, ErrorCause::Error(message)))?;
        let directories: Vec<_> = parsed
            .directories
            .iter()
            .map(|dir| {
                let absolute = file_access::resolve_path(&target.absolute, dir, &ctx.global.home);
                file_access::resolve(ctx, &absolute.to_string_lossy(), Some(Kind::Directory))
            })
            .collect();
        let mut targets = vec![&target];
        targets.extend(directories.iter());
        file_access::authorize_external(ctx, &targets, None).await.map_err(super::perm(leaf))?;
        if !parsed.commands.is_empty() {
            super::assert(
                ctx,
                "shell",
                parsed.commands.iter().map(|c| c.resource.clone()).collect(),
                Some(parsed.commands.iter().map(|c| c.save.clone()).collect()),
                None,
            )
            .await
            .map_err(super::perm(leaf))?;
        }
        let abs = target.absolute.to_string_lossy().into_owned();
        match std::fs::metadata(&target.absolute) {
            Err(_) => return Err(ToolError::with_cause(leaf, ErrorCause::Error(format!("Working directory does not exist: {abs}")))),
            Ok(m) if !m.is_dir() => {
                return Err(ToolError::with_cause(leaf, ErrorCause::Error(format!("Working directory is not a directory: {abs}"))));
            }
            Ok(_) => {}
        }
        Ok(timeout)
    }
}

/// A shell command's job run and the structured result it settles with (`startJob`).
pub struct ShellJob {
    pub run: futures::future::BoxFuture<'static, Result<String, String>>,
    pub settled: tokio::sync::watch::Receiver<Option<ShellOutput>>,
}

/// `startJob`'s run: waits for the command's result and renders it. Dropping the run before it settles (its job
/// cancelled or its registry closing) stops the command, keeping it and its output file, unless the server is
/// handing its daemon over. `timeout` is the call's final timeout, for the timed-out notice.
pub fn job(service: Arc<ShellService>, started: oc_schema::message::ShellInfo, max_lines: usize, max_bytes: usize, timeout: Option<Arc<AtomicU64>>) -> ShellJob {
    let (settled_tx, settled) = tokio::sync::watch::channel::<Option<ShellOutput>>(None);
    let guard = StopOnDrop { service: service.clone(), id: started.id.clone(), armed: true, job: None };
    let run = async move {
        // The whole guard moves into the run, so dropping the run drops it.
        let mut guard = guard;
        let result = service.result(&started, max_lines, max_bytes).await;
        guard.armed = false;
        if result.capture.is_none() {
            return Err("Command failed".to_string());
        }
        let completed = result.info.time.completed.unwrap_or_else(crate::util::now_ms);
        let truncated = result.capture.as_ref().is_some_and(|c| c.truncated);
        let output = ShellOutput {
            base: shell_result::output(&result),
            wall_time_seconds: wall_time_seconds(result.info.time.started, completed),
            output_path: truncated.then(|| result.info.file.clone()),
            ..Default::default()
        };
        let text = result_messages(&output, timeout.map(|t| t.load(Ordering::SeqCst))).join("\n\n");
        let _ = settled_tx.send(Some(output));
        Ok(text)
    };
    ShellJob { run: Box::pin(run), settled }
}

/// `backgroundToolResult`: the tool result of a call whose command moved to the background, narrowed to what a
/// backgrounded command reports so it can be published as is: its content and metadata.
pub fn background_tool_result(info: &oc_schema::message::ShellInfo) -> (Vec<ToolContent>, oc_schema::Metadata) {
    let mut metadata = meta(vec![("shellID", json!(info.id)), ("status", json!("running")), ("truncated", json!(false))]);
    if let Some(pid) = info.pid {
        metadata.insert("pid".into(), json!(pid));
    }
    let content = [background_text(info, None), BACKGROUND_INSTRUCTION.to_string()].into_iter().map(|text| ToolContent::Text { text }).collect();
    (content, metadata)
}

/// The notice a background command's settled job leaves its session (`notify`): the job's rendered output when it
/// completed, its error, or `Cancelled`.
pub fn job_notice(shell_id: &str, job: &crate::host::JobInfo, command: &str, settled: Option<&ShellOutput>) -> shell_result::Notification {
    let text = match (settled, job.status) {
        (Some(_), _) => job.output.clone().unwrap_or_default(),
        (None, JobStatus::Error) => job.error.clone().unwrap_or_else(|| "Command failed".into()),
        _ => "Cancelled".into(),
    };
    shell_result::notification(shell_id, Some(&job.id), command, job.status.as_str(), &text, settled.map(|o| &o.base))
}

fn spawn_notifier(ctx: &ToolContext, job_id: String, shell_id: String, command: String, settled: tokio::sync::watch::Receiver<Option<ShellOutput>>) {
    let host = ctx.host.clone();
    let session_id = ctx.session_id.clone();
    tokio::spawn(async move {
        let Ok(Some(info)) = host.job_wait(&job_id).await else { return };
        if info.status == JobStatus::Running {
            return;
        }
        let output = if info.status == JobStatus::Completed {
            let mut rx = settled;
            loop {
                if let Some(out) = rx.borrow().clone() {
                    break Some(out);
                }
                if rx.changed().await.is_err() {
                    break None;
                }
            }
        } else {
            None
        };
        let note = job_notice(&shell_id, &info, &command, output.as_ref());
        let _ = host
            .session_synthetic(SyntheticMessage {
                id: info.notification_id.clone(),
                session_id,
                description: command.clone(),
                text: note.text,
                metadata: note.metadata,
                unlocated: true,
            })
            .await;
        if let Some(id) = &info.notification_id {
            let _ = host.job_complete_background(id).await;
        }
    });
}

#[async_trait]
impl Tool for ShellTool {
    fn name(&self) -> &str {
        "shell"
    }

    fn options(&self) -> ToolOptions {
        ToolOptions::direct()
    }

    fn definition(&self, ctx: &DefinitionContext) -> ToolDefinition {
        let shell = ctx.shell.as_deref().map(select::name);
        super::definition("shell", description(shell.as_deref()), include_str!("../../schemas/shell.json"))
    }

    async fn execute(&self, input: Value, ctx: ToolContext) -> Result<ToolResult, ToolError> {
        let input: Input = decode(input)?;
        // Listens for the user's stop for the whole call, starting before the command does, so a stop always reaches
        // this tool instead of interrupting it, even while the command starts.
        let listener = ctx.stop.listen();
        let stopped = || {
            let listener = listener.as_ref();
            async move {
                match listener {
                    Some(listener) => listener.stopped().await,
                    None => std::future::pending().await,
                }
            }
        };
        let leaf = format!("Unable to execute command: {}", input.command);
        let background = input.background == Some(true);
        // A foreground timeout of 0 falls back to the default, so a call can't block until the command exits.
        let timeout = if background { input.timeout.unwrap_or(0) } else { input.timeout.filter(|t| *t > 0).unwrap_or(DEFAULT_TIMEOUT_MS) };
        let shell = Self::compatible_shell(&ctx);
        let base_env: HashMap<String, String> = if ctx.workspace_id.is_some() {
            HashMap::new()
        } else {
            match ctx.host.session_environment(&ctx.session_id).await {
                Some(env) => env,
                None => std::env::vars().collect(),
            }
        };
        let mut invocation = ShellInvocation {
            command: input.command.clone(),
            cwd: input.workdir.clone().unwrap_or_else(|| ctx.directory.to_string_lossy().into_owned()),
            timeout,
            shell,
            env: base_env,
        };
        invocation.env.insert("TERM".into(), "xterm-256color".into());
        invocation.env.insert("OPENCODE_TERMINAL".into(), "1".into());
        ctx.host.shell_create_before(&mut invocation).await.map_err(|m| ToolError::with_cause(leaf.clone(), ErrorCause::Error(m)))?;
        invocation.env.insert("AGENT".into(), "1".into());
        invocation.env.insert("OPENCODE".into(), "1".into());
        if invocation.env.get("AI_AGENT").is_none_or(|v| v.is_empty()) {
            invocation.env.insert("AI_AGENT".into(), "opencode".into());
        }
        invocation.env.insert("OPENCODE_SESSION_ID".into(), ctx.session_id.clone());
        // Asking for permission can take a while; a stop then ends the call before any command starts.
        let final_timeout = tokio::select! {
            biased;
            _ = stopped() => return Err(ToolError::new("Command interrupted by the user before it started.")),
            prepared = self.prepare(&mut invocation, &ctx, &leaf) => prepared?,
        };
        // A foreground timeout moves the command to the background instead of stopping it.
        if !background {
            invocation.timeout = 0;
        }

        let placement = Placement {
            project_id: ctx.project_id.clone(),
            directory: ctx.directory.to_string_lossy().into_owned(),
            workspace_id: ctx.workspace_id.clone(),
            tool_call: Some(ToolCall {
                session_id: ctx.session_id.clone(),
                assistant_message_id: ctx.message_id.clone(),
                call_id: ctx.call_id.clone(),
            }),
            waited: !background,
        };
        let info = self
            .service
            .create(&invocation, meta(vec![("sessionID", json!(ctx.session_id))]), &placement)
            .await
            .map_err(|m| ToolError::with_cause(leaf.clone(), ErrorCause::Error(m)))?;
        ctx.progress(meta(vec![("shellID", json!(info.id))]));
        // Settled or not, the call ends here unless it is dropped (interrupted) first.
        let mut interrupted = StopOnDrop { service: self.service.clone(), id: info.id.clone(), armed: true, job: None };
        let ended = self.started(info, background, final_timeout, leaf, listener.as_ref(), &mut interrupted, ctx).await;
        interrupted.armed = false;
        ended
    }
}

impl ShellTool {
    /// The call once its command started: starts its job, then moves it to the background or waits for it.
    #[allow(clippy::too_many_arguments)]
    async fn started(
        &self,
        info: oc_schema::message::ShellInfo,
        background: bool,
        final_timeout: u64,
        leaf: String,
        listener: Option<&crate::tool::StopListener>,
        interrupted: &mut StopOnDrop,
        ctx: ToolContext,
    ) -> Result<ToolResult, ToolError> {
        let stopped = || async move {
            match listener {
                Some(listener) => listener.stopped().await,
                None => std::future::pending().await,
            }
        };
        // `finalTimeout`: the limit the invocation ended up with, cleared once the command moves to the background,
        // so the notices say how long it waited.
        let final_timeout = Arc::new(AtomicU64::new(final_timeout));
        let ShellJob { run, settled: settled_rx } =
            job(self.service.clone(), info.clone(), ctx.config.max_lines(), ctx.config.max_bytes(), Some(final_timeout.clone()));
        let job = ctx
            .host
            .job_start(JobStart {
                id: info.id.clone(),
                kind: "shell".into(),
                title: Some(info.command.clone()),
                metadata: meta(vec![("sessionID", json!(ctx.session_id)), ("shellID", json!(info.id))]),
                recovery: Some(JobRecovery::Shell { session_id: ctx.session_id.clone(), shell_id: info.id.clone(), command: info.command.clone() }),
                run,
            })
            .await
            .map_err(|m| ToolError::with_cause(leaf.clone(), ErrorCause::Error(m)))?;
        interrupted.job = Some((ctx.host.clone(), job.id.clone()));
        // Stopped while the command was starting: stop it now.
        if listener.is_some_and(|l| l.stopped().now_or_never().is_some()) {
            return Err(self.cancelled(&ctx, &info, &job.id).await);
        }

        if background {
            let _ = ctx.host.job_background(&job.id).await;
            spawn_notifier(&ctx, job.id.clone(), info.id.clone(), info.command.clone(), settled_rx);
            return Ok(tool_result(background_result(&info, crate::util::now_ms(), None), None));
        }

        // Past its timeout, a foreground command moves to the background (#63699).
        let limit = final_timeout.load(Ordering::SeqCst);
        let deadline = (limit > 0).then(|| tokio::time::Instant::now() + std::time::Duration::from_millis(limit));
        let expired = || async move {
            match deadline {
                Some(deadline) => tokio::time::sleep_until(deadline).await,
                None => std::future::pending().await,
            }
        };
        // A user's stop arrives through `stop`; an interrupt (a server shutdown) cancels the job, which stops the
        // command and keeps it, output file included.
        let blocked = tokio::select! {
            result = ctx.host.job_block(&job.id, &ctx.session_id) => result,
            _ = expired() => match ctx.host.job_background(&job.id).await {
                Ok(Some(_)) => return Ok(self.timed_out(&ctx, &job.id, &info, limit, &final_timeout, settled_rx)),
                _ => Ok(None),
            },
            _ = stopped() => return Err(self.cancelled(&ctx, &info, &job.id).await),
            _ = ctx.cancel.cancelled() => return Err(self.interrupted(&ctx, &job.id, &info).await),
        };
        match blocked {
            Ok(Some(JobBlockResult::Backgrounded(_))) => {
                let _ = self.service.timeout(&info.id, 0);
                final_timeout.store(0, Ordering::SeqCst);
                spawn_notifier(&ctx, job.id.clone(), info.id.clone(), info.command.clone(), settled_rx);
                return Ok(tool_result(background_result(&info, crate::util::now_ms(), None), None));
            }
            Ok(Some(JobBlockResult::Finished(job_info))) if job_info.status == JobStatus::Error => {
                return Err(ToolError::with_cause(leaf, ErrorCause::Error(job_info.error.unwrap_or_else(|| "Command failed".into()))));
            }
            Ok(Some(JobBlockResult::Finished(job_info))) if job_info.status == JobStatus::Cancelled => {
                return Err(ToolError::with_cause(leaf, ErrorCause::Error("Command cancelled".into())));
            }
            _ => {}
        }
        let mut rx = settled_rx;
        let output = loop {
            if let Some(out) = rx.borrow().clone() {
                break out;
            }
            tokio::select! {
                changed = rx.changed() => {
                    if changed.is_err() {
                        return Err(ToolError::with_cause(leaf, ErrorCause::Error("Command failed".into())));
                    }
                }
                // A host without a job registry returns from `job_block` at once; the stop and the timeout still
                // reach the call.
                _ = stopped() => return Err(self.cancelled(&ctx, &info, &job.id).await),
                _ = expired(), if deadline.is_some_and(|d| d > tokio::time::Instant::now()) => {
                    if let Ok(Some(_)) = ctx.host.job_background(&job.id).await {
                        return Ok(self.timed_out(&ctx, &job.id, &info, limit, &final_timeout, rx));
                    }
                }
                _ = ctx.cancel.cancelled() => return Err(self.interrupted(&ctx, &job.id, &info).await),
            }
        };
        Ok(tool_result(output, Some(final_timeout.load(Ordering::SeqCst))))
    }

    /// An interrupt (other than the user's stop) cancels the job, which stops the command, unless the server is
    /// handing its daemon over: then the command and its job keep running for the next server.
    async fn interrupted(&self, ctx: &ToolContext, job_id: &str, info: &oc_schema::message::ShellInfo) -> ToolError {
        if !self.service.handoff_prepared() {
            let _ = ctx.host.job_cancel(job_id).await;
            let _ = self.service.stop(&info.id, oc_schema::message::ShellStatus::Killed);
        }
        ToolError::interrupted()
    }

    /// The call of a foreground command its timeout moved to the background: the command runs on without a limit
    /// and the session hears when it ends.
    fn timed_out(
        &self,
        ctx: &ToolContext,
        job_id: &str,
        info: &oc_schema::message::ShellInfo,
        limit: u64,
        final_timeout: &AtomicU64,
        settled: tokio::sync::watch::Receiver<Option<ShellOutput>>,
    ) -> ToolResult {
        let _ = self.service.timeout(&info.id, 0);
        final_timeout.store(0, Ordering::SeqCst);
        spawn_notifier(ctx, job_id.to_string(), info.id.clone(), info.command.clone(), settled);
        tool_result(background_result(info, crate::util::now_ms(), Some(limit)), None)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn descriptions() {
        assert!(!description(None).contains("Commands run on"));
        // 2.3.1 (#63699, #63718).
        assert_eq!(
            description(None),
            "Execute a shell command and return its output. Quote file paths containing spaces or special characters. Prefer dedicated tools over shell commands when possible. When output is large, the full result is saved to a file and a truncated preview is returned. Rely on automatic truncation unless filtering the output is more useful. Commands still running after their timeout move to the background. Background commands return immediately, and you will be notified when they complete. The current session ID is available in the OPENCODE_SESSION_ID environment variable."
        );
        assert!(description(Some("bash")).contains(&format!("Commands run on {} using bash. Quote", os_name())));
    }

    fn info(pid: Option<u32>) -> oc_schema::message::ShellInfo {
        oc_schema::message::ShellInfo {
            id: "sh_1".into(),
            status: oc_schema::message::ShellStatus::Running,
            command: "sleep 1".into(),
            cwd: "/d".into(),
            shell: "/bin/bash".into(),
            file: "/d/sh_1.out".into(),
            pid,
            exit: None,
            signal: None,
            metadata: Default::default(),
            time: oc_schema::message::ShellTime { started: 1_000, completed: None },
        }
    }

    /// tool-shell.test.ts (2.3.0): a backgrounded command reports its PID, output file and elapsed time.
    #[test]
    fn background_content() {
        let result = tool_result(background_result(&info(Some(4242)), 3_500, None), None);
        assert_eq!(
            result.content,
            vec![
                ToolContent::Text { text: "Command moved to the background (shell ID: sh_1).\nPID: 4242\nOutput is streaming to: /d/sh_1.out".into() },
                ToolContent::Text { text: BACKGROUND_INSTRUCTION.into() },
            ]
        );
        assert_eq!(serde_json::to_string(&result.metadata).unwrap(), r#"{"status":"running","truncated":false,"shellID":"sh_1","pid":4242}"#);
        assert_eq!(
            serde_json::to_string(&result.output).unwrap(),
            r#"{"shellID":"sh_1","exitCode":null,"output":"Command moved to the background (shell ID: sh_1).\nPID: 4242\nOutput is streaming to: /d/sh_1.out","outputPath":"/d/sh_1.out","pid":4242,"wallTimeSeconds":2.5}"#
        );
        let without = tool_result(background_result(&info(None), 500, None), None);
        assert_eq!(
            without.content[0],
            ToolContent::Text { text: "Command moved to the background (shell ID: sh_1).\nOutput is streaming to: /d/sh_1.out".into() }
        );
        assert_eq!(without.output.unwrap()["wallTimeSeconds"], json!(0));
        // tool-shell.test.ts (2.3.1, #63699): a foreground timeout says how long the command had.
        let timed_out = tool_result(background_result(&info(Some(7)), 1_010, Some(10)), None);
        assert_eq!(
            timed_out.content[0],
            ToolContent::Text {
                text: "Command moved to the background (shell ID: sh_1).\nPID: 7\nOutput is streaming to: /d/sh_1.out\n\nThe command did not finish within its timeout of 0.01 seconds and was moved to the background. Do not start another copy while it is running.".into()
            }
        );
        assert!(background_text(&info(None), Some(120_000)).ends_with("timeout of 120 seconds and was moved to the background. Do not start another copy while it is running."));
    }

    #[test]
    fn completed_content() {
        let output = ShellOutput { base: Output { output: "x".into(), exit: Some(1), ..Default::default() }, wall_time_seconds: 2.0, ..Default::default() };
        let result = tool_result(output, Some(120_000));
        assert_eq!(result.content, vec![ToolContent::Text { text: "x".into() }, ToolContent::Text { text: "Exit code: 1".into() }]);
        assert_eq!(serde_json::to_string(&result.metadata).unwrap(), r#"{"status":"completed","truncated":false,"exit":1}"#);
        assert_eq!(serde_json::to_string(&result.output).unwrap(), r#"{"exitCode":1,"output":"x","wallTimeSeconds":2}"#);
    }

    /// A signalled command has no exit code; a cut output names its file.
    #[test]
    fn signalled_and_truncated_output() {
        let output = ShellOutput {
            base: Output { output: "y".into(), truncated: true, signal: Some("SIGTERM".into()), ..Default::default() },
            wall_time_seconds: 0.25,
            output_path: Some("/d/sh_1.out".into()),
            ..Default::default()
        };
        assert_eq!(
            serde_json::to_string(&tool_result(output, None).output).unwrap(),
            r#"{"exitCode":null,"signal":"SIGTERM","output":"y","outputPath":"/d/sh_1.out","wallTimeSeconds":0.25}"#
        );
    }
}
