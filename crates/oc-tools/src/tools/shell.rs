//! `shell` (`core/src/tool/plugin/shell.ts`).

use std::collections::HashMap;
use std::sync::Arc;

use async_trait::async_trait;
use oc_schema::ToolContent;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::file_access::{self, Kind};
use crate::host::{JobBlockResult, JobRecovery, JobStart, JobStatus, ShellInvocation, SyntheticMessage};
use crate::shell::result::{self as shell_result, Output};
use crate::shell::service::ShellService;
use crate::shell::{parse, select};
use crate::tool::{DefinitionContext, ErrorCause, Tool, ToolContext, ToolDefinition, ToolError, ToolOptions, ToolResult, decode, meta};

pub const DEFAULT_TIMEOUT_MS: u64 = 2 * 60 * 1_000;

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
            "Commands accept an optional timeout, background commands have no timeout by default.",
            "Background commands return immediately, and you will be notified when they complete.",
        ]
        .map(String::from),
    );
    parts.join(" ")
}

#[derive(Debug, Clone, PartialEq, Default)]
struct ShellOutput {
    base: Output,
    shell_id: Option<String>,
    running: bool,
}

fn result_messages(output: &ShellOutput) -> Vec<String> {
    let notice = if output.running { Some(BACKGROUND_INSTRUCTION.to_string()) } else { shell_result::notice(&output.base) };
    let mut out = Vec::new();
    if !output.base.output.is_empty() {
        out.push(output.base.output.clone());
    }
    if let Some(n) = notice {
        out.push(n);
    }
    out
}

fn tool_result(output: ShellOutput) -> ToolResult {
    let mut metadata = meta(vec![("status", json!(if output.running { "running" } else { "completed" }))]);
    for (k, v) in shell_result::metadata(&output.base) {
        metadata.insert(k, v);
    }
    if let Some(id) = &output.shell_id {
        metadata.insert("shellID".into(), json!(id));
    }
    let mut value = shell_result::value(&output.base);
    if let Some(id) = &output.shell_id {
        value["shellID"] = json!(id);
    }
    value["status"] = json!(if output.running { "running" } else { "completed" });
    ToolResult {
        content: result_messages(&output).into_iter().map(|text| ToolContent::Text { text }).collect(),
        output: Some(value),
        metadata: Some(metadata),
    }
}

fn background_result(shell_id: &str, file: &str) -> ShellOutput {
    ShellOutput {
        base: Output {
            output: format!("Command moved to the background (shell ID: {shell_id}).\nOutput is streaming to: {file}"),
            truncated: false,
            ..Default::default()
        },
        shell_id: Some(shell_id.to_string()),
        running: true,
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

    async fn prepare(&self, invocation: &mut ShellInvocation, ctx: &ToolContext, leaf: &str) -> Result<u64, ToolError> {
        let target = file_access::resolve(ctx, &invocation.cwd, Some(Kind::Directory));
        invocation.cwd = target.absolute.to_string_lossy().into_owned();
        let timeout = invocation.timeout;
        let portable = ctx.config.portable_shell_scanner.unwrap_or(ctx.config.channel == "local" || ctx.config.channel == "dev");
        let parsed = parse::scan(&invocation.command, &invocation.shell, &invocation.cwd, &ctx.global.home, portable)
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
        let text = match (&output, info.status) {
            (Some(out), _) => result_messages(out).join("\n\n"),
            (None, JobStatus::Error) => info.error.clone().unwrap_or_else(|| "Command failed".into()),
            _ => "Cancelled".into(),
        };
        let note = shell_result::notification(&shell_id, Some(&job_id), &command, info.status.as_str(), &text, output.as_ref().map(|o| &o.base));
        let _ = host
            .session_synthetic(SyntheticMessage {
                id: info.notification_id.clone(),
                session_id,
                description: command.clone(),
                text: note.text,
                metadata: note.metadata,
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
        let leaf = format!("Unable to execute command: {}", input.command);
        let background = input.background == Some(true);
        let timeout = if background { input.timeout.unwrap_or(0) } else { input.timeout.unwrap_or(DEFAULT_TIMEOUT_MS) };
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
        let final_timeout = self.prepare(&mut invocation, &ctx, &leaf).await?;

        let info = self
            .service
            .spawn(&invocation, meta(vec![("sessionID", json!(ctx.session_id))]), &ctx.project_id)
            .await
            .map_err(|m| ToolError::with_cause(leaf.clone(), ErrorCause::Error(m)))?;
        ctx.progress(meta(vec![("shellID", json!(info.id))]));

        let (settled_tx, settled_rx) = tokio::sync::watch::channel::<Option<ShellOutput>>(None);
        let service = self.service.clone();
        let started = info.clone();
        let max_lines = ctx.config.max_lines();
        let max_bytes = ctx.config.max_bytes();
        let run = async move {
            let result = service.result(&started, max_lines, max_bytes).await;
            if result.capture.is_none() {
                return Err("Command failed".to_string());
            }
            let mut base = shell_result::output(&result);
            if base.timeout == Some(true) {
                base.output = format!(
                    "{}\n\nCommand exceeded timeout of {final_timeout} ms. Retry with a larger timeout if the command is expected to take longer.",
                    base.output
                );
            }
            let output = ShellOutput { base, shell_id: None, running: false };
            let text = result_messages(&output).join("\n\n");
            let _ = settled_tx.send(Some(output));
            Ok(text)
        };
        let job = ctx
            .host
            .job_start(JobStart {
                id: info.id.clone(),
                kind: "shell".into(),
                title: Some(info.command.clone()),
                metadata: meta(vec![("sessionID", json!(ctx.session_id)), ("shellID", json!(info.id))]),
                recovery: Some(JobRecovery::Shell { session_id: ctx.session_id.clone(), shell_id: info.id.clone(), command: info.command.clone() }),
                run: Box::pin(run),
            })
            .await
            .map_err(|m| ToolError::with_cause(leaf.clone(), ErrorCause::Error(m)))?;

        if background {
            let _ = ctx.host.job_background(&job.id).await;
            spawn_notifier(&ctx, job.id.clone(), info.id.clone(), info.command.clone(), settled_rx);
            return Ok(tool_result(background_result(&info.id, &info.file)));
        }

        let blocked = tokio::select! {
            result = ctx.host.job_block(&job.id, &ctx.session_id) => result,
            _ = ctx.cancel.cancelled() => {
                let _ = ctx.host.job_cancel(&job.id).await;
                let _ = self.service.remove(&info.id);
                return Err(ToolError::interrupted());
            }
        };
        match blocked {
            Ok(Some(JobBlockResult::Backgrounded(_))) => {
                let _ = self.service.timeout(&info.id, 0);
                spawn_notifier(&ctx, job.id.clone(), info.id.clone(), info.command.clone(), settled_rx);
                return Ok(tool_result(background_result(&info.id, &info.file)));
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
                _ = ctx.cancel.cancelled() => {
                    let _ = ctx.host.job_cancel(&job.id).await;
                    let _ = self.service.remove(&info.id);
                    return Err(ToolError::interrupted());
                }
            }
        };
        Ok(tool_result(output))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn descriptions() {
        assert!(!description(None).contains("Commands run on"));
        assert!(description(Some("bash")).contains(&format!("Commands run on {} using bash. Quote", os_name())));
    }

    #[test]
    fn background_content() {
        let result = tool_result(background_result("sh_1", "/d/sh_1.out"));
        assert_eq!(
            result.content,
            vec![
                ToolContent::Text { text: "Command moved to the background (shell ID: sh_1).\nOutput is streaming to: /d/sh_1.out".into() },
                ToolContent::Text { text: BACKGROUND_INSTRUCTION.into() },
            ]
        );
        assert_eq!(serde_json::to_string(&result.metadata).unwrap(), r#"{"status":"running","truncated":false,"shellID":"sh_1"}"#);
    }

    #[test]
    fn completed_content() {
        let result = tool_result(ShellOutput { base: Output { output: "x".into(), exit: Some(1), ..Default::default() }, shell_id: None, running: false });
        assert_eq!(result.content, vec![ToolContent::Text { text: "x".into() }, ToolContent::Text { text: "Exited with code 1".into() }]);
        assert_eq!(serde_json::to_string(&result.metadata).unwrap(), r#"{"status":"completed","truncated":false,"exit":1}"#);
    }
}
