//! User shell commands (`!` prefix): `Session.shell` (`core/session/session.ts:179-222`,
//! `core/session/shell.ts`, `core/shell/result.ts`).

use std::sync::Arc;

use oc_schema::event::SessionEvent;
use oc_tools::host::ShellInvocation;
use oc_tools::shell::{result, select};

use crate::bus::Durable1;
use crate::runner::Engine;

impl Engine {
    /// Runs a user shell command to completion, recording shell started/ended and the synthetic notice.
    pub async fn session_shell(self: &Arc<Self>, session_id: &str, id: Option<String>, command: &str) -> anyhow::Result<()> {
        let session = self.app.sessions.get(session_id)?;
        let location = self.location(&session.location.directory).await?;
        let shell = select::resolve(select::Priority::Config, oc_config::fold::shell(&location.entries), Some(&self.app.global.bin));
        let mut env: std::collections::HashMap<String, String> = if session.location.workspace_id.is_some() {
            Default::default()
        } else {
            self.app.environments.lock().get(session_id).cloned().unwrap_or_else(|| std::env::vars().collect())
        };
        env.insert("TERM".into(), "xterm-256color".into());
        env.insert("OPENCODE_TERMINAL".into(), "1".into());
        let invocation = ShellInvocation { command: command.into(), cwd: session.location.directory.clone(), timeout: 0, shell, env };
        let mut metadata = serde_json::Map::new();
        metadata.insert("sessionID".into(), session_id.into());
        metadata.insert("background".into(), true.into());
        let started = match self.shell.spawn(&invocation, metadata, &session.project_id).await {
            Ok(info) => info,
            Err(e) => {
                let mut meta = serde_json::Map::new();
                meta.insert("source".into(), "shell".into());
                meta.insert("state".into(), "error".into());
                self.app
                    .sessions
                    .synthetic(session_id, None, format!("User shell command failed to start:\n{command}\n\n{e}"), Some(command.into()), Some(meta), None)
                    .await?;
                anyhow::bail!(e);
            }
        };
        self.app.bus.commit(vec![Durable1 {
            event: SessionEvent::ShellStarted { session_id: session_id.into(), shell: started.clone() },
            metadata: None,
            id: id.map(|m| m.replacen("msg_", "evt_", 1)),
        }])?;
        let entries = &location.entries;
        let max_lines = oc_config::fold::tool_output(entries).and_then(|t| t.max_lines).unwrap_or(2000) as usize;
        let max_bytes = oc_config::fold::tool_output(entries).and_then(|t| t.max_bytes).unwrap_or(51200) as usize;
        let terminal = self.shell.result(&started, max_lines, max_bytes).await;
        let preview = self.shell.output(&started.id, None, Some(1024 * 1024)).unwrap_or(oc_schema::message::ShellOutput {
            output: result::MISSING.into(),
            cursor: 0,
            size: 0,
            truncated: false,
        });
        self.app.bus.commit_one(SessionEvent::ShellEnded { session_id: session_id.into(), shell: terminal.info.clone(), output: preview })?;
        let note = result::user_notification(&terminal);
        // `resume: false`: delivered at the next drain, without waking.
        let _ = self.app.sessions.synthetic(session_id, None, note.text, None, Some(note.metadata), None).await;
        Ok(())
    }
}
