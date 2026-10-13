//! `ToolHost` implementation: how tools call back into core.

use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;
use serde_json::Value;

use oc_schema::types::ModelRef;
use oc_tools::host::*;

use crate::app::App;
use crate::permission::{AssertInput, PermissionError as CoreError, Source};

pub struct CoreHost {
    pub app: Arc<App>,
    /// Skills visible to this session's location.
    pub skills: Arc<Vec<SkillInfo>>,
    /// MCP manager of the session's location.
    pub mcp: Option<Arc<oc_mcp::manager::McpManager>>,
    /// Plugin location key of the session (`shell.create.before`).
    pub location: Option<String>,
}

#[async_trait]
impl ToolHost for CoreHost {
    async fn ask_permission(&self, r: PermissionRequest) -> Result<(), PermissionError> {
        let input = AssertInput {
            id: None,
            session_id: r.session_id,
            action: r.action,
            resources: r.resources,
            save: r.save,
            metadata: r.metadata,
            source: Some(Source { kind: "tool".into(), message_id: r.source.message_id, id: r.source.call_id }),
            agent: r.agent,
        };
        self.app.permissions.assert(input).await.map_err(|e| match e {
            CoreError::Declined => PermissionError::Declined,
            CoreError::Corrected { feedback } => PermissionError::Corrected { feedback },
            blocked @ CoreError::Blocked { .. } => PermissionError::Blocked { message: blocked.to_string() },
        })
    }

    async fn ask_form(&self, request: FormRequest) -> Result<FormResponse, String> {
        let input = serde_json::json!({
            "sessionID": request.session_id,
            "title": request.title,
            "metadata": request.metadata,
            "fields": request.fields,
        });
        match self.app.forms.ask(&input).await.map_err(|e| e.to_string())? {
            crate::form::State::Answered(a) => Ok(FormResponse::Answered(a)),
            crate::form::State::Cancelled(message) => Ok(FormResponse::Cancelled { message }),
            crate::form::State::Pending => Ok(FormResponse::Cancelled { message: None }),
        }
    }

    async fn load_instructions(&self, session_id: &str, paths: Vec<PathBuf>) -> Result<(), String> {
        let session = self.app.sessions.get(session_id).map_err(|e| e.to_string())?;
        // Dedupe against paths already loaded into current history.
        let loaded: Vec<String> = self
            .app
            .db
            .session_messages(session_id)
            .map_err(|e| e.to_string())?
            .into_iter()
            .filter_map(|(_, m)| match m {
                oc_schema::Message::Synthetic { metadata: Some(meta), .. } => meta
                    .get("instruction")
                    .and_then(|i| i.get("paths"))
                    .and_then(Value::as_array)
                    .map(|a| a.iter().filter_map(|p| p.as_str().map(str::to_string)).collect::<Vec<_>>()),
                _ => None,
            })
            .flatten()
            .collect();
        let files: Vec<crate::instructions::File> = paths
            .into_iter()
            .map(|p| p.display().to_string())
            .filter(|p| !loaded.contains(p))
            .filter_map(|p| std::fs::read_to_string(&p).ok().map(|content| crate::instructions::File { path: p, content }))
            .collect();
        if files.is_empty() {
            return Ok(());
        }
        let project = crate::project::get(&self.app.bus, &session.project_id)
            .ok()
            .flatten()
            .and_then(|p| p.get("canonical").and_then(Value::as_str).map(PathBuf::from))
            .unwrap_or_else(|| PathBuf::from(&session.location.directory));
        let (text, description, metadata) = crate::discovery::nested_synthetic(&files, &project);
        self.app
            .bus
            .commit_one(oc_schema::event::SessionEvent::Synthetic {
                session_id: session_id.into(),
                text,
                description: Some(description),
                metadata: metadata.as_object().cloned(),
            })
            .map_err(|e| e.to_string())?;
        Ok(())
    }

    async fn get_skill(&self, id: &str) -> Result<Option<SkillInfo>, String> {
        Ok(self.skills.iter().find(|s| s.id == id).cloned())
    }

    async fn list_skills(&self) -> Result<Vec<SkillInfo>, String> {
        Ok(self.skills.as_ref().clone())
    }

    async fn mcp_servers(&self) -> Result<Vec<String>, String> {
        Ok(self.mcp.as_ref().map(|m| m.servers().into_iter().map(|s| s.name).collect()).unwrap_or_default())
    }

    async fn mcp_resource_catalog(&self) -> Result<Value, String> {
        let Some(m) = &self.mcp else { return Err(unsupported("MCP")) };
        serde_json::to_value(m.resource_catalog().await).map_err(|e| e.to_string())
    }

    async fn mcp_resources(&self, server: &str) -> Result<Value, String> {
        let Some(m) = &self.mcp else { return Err(unsupported("MCP")) };
        let r = m.resources(server).await.map_err(|e| e.to_string())?;
        serde_json::to_value(r).map_err(|e| e.to_string())
    }

    async fn mcp_read_resource(&self, server: &str, uri: &str) -> Result<Option<Value>, String> {
        let Some(m) = &self.mcp else { return Err(unsupported("MCP")) };
        let r = m.read_resource(server, uri).await.map_err(|e| e.to_string())?;
        Ok(r.map(|r| serde_json::to_value(r).unwrap_or(Value::Null)))
    }

    async fn mcp_call_tool(&self, server: &str, name: &str, args: Value, session_id: &str) -> Result<McpCallResult, McpCallError> {
        let Some(m) = &self.mcp else { return Err(McpCallError::Other(unsupported("MCP"))) };
        match m.call_tool(server, name, Some(args), Some(session_id)).await {
            Ok(r) => Ok(McpCallResult {
                content: r
                    .content
                    .into_iter()
                    .map(|c| match c {
                        oc_mcp::types::CallToolContent::Text { text } => McpContent::Text(text),
                        oc_mcp::types::CallToolContent::Media { data, mime_type } => McpContent::Data { data, mime: mime_type },
                    })
                    .collect(),
                structured: r.structured,
                is_error: r.is_error,
            }),
            Err(oc_mcp::manager::ManagerError::NotFound(s)) => Err(McpCallError::NotFound { server: s }),
            Err(oc_mcp::manager::ManagerError::ToolCall { message, .. }) => Err(McpCallError::ToolCall { message }),
            Err(e) => Err(McpCallError::Other(e.to_string())),
        }
    }

    async fn session_get(&self, session_id: &str) -> Result<SessionSummary, String> {
        let s = self.app.sessions.get(session_id).map_err(|e| e.to_string())?;
        Ok(SessionSummary { id: s.id, parent_id: s.parent_id, agent: s.agent, model: s.model })
    }

    async fn session_create(
        &self,
        parent_id: &str,
        title: &str,
        agent: &str,
        model: Option<ModelRef>,
    ) -> Result<SessionSummary, String> {
        let s = self
            .app
            .sessions
            .create(crate::session::CreateInput {
                parent_id: Some(parent_id.into()),
                title: Some(title.into()),
                agent: Some(agent.into()),
                model,
                ..Default::default()
            })
            .await
            .map_err(|e| e.to_string())?;
        Ok(SessionSummary { id: s.id, parent_id: s.parent_id, agent: s.agent, model: s.model })
    }

    async fn session_switch_agent(&self, session_id: &str, agent: &str) -> Result<(), String> {
        self.app.sessions.switch_agent(session_id, agent).map_err(|e| e.to_string())
    }

    async fn session_switch_model(&self, session_id: &str, model: ModelRef) -> Result<(), String> {
        self.app.sessions.switch_model(session_id, model).map_err(|e| e.to_string())
    }

    async fn session_prompt(&self, session_id: &str, text: &str, resume: Option<bool>) -> Result<(), String> {
        self.app
            .sessions
            .prompt(session_id, crate::session::PromptInput { text: text.into(), ..Default::default() })
            .await
            .map_err(|e| e.to_string())?;
        if resume != Some(false) {
            if let Some(c) = self.app.coordinator.get() {
                c.wake(session_id, crate::inbox::Scope::Input);
            }
        }
        Ok(())
    }

    async fn session_interrupt(&self, session_id: &str) -> Result<(), String> {
        if let Some(c) = self.app.coordinator.get() {
            c.interrupt(session_id, Some(crate::coordinator::Reason::User), false).await;
        }
        Ok(())
    }

    async fn session_rename(&self, session_id: &str, title: &str) -> Result<(), String> {
        self.app.sessions.rename(session_id, title).map_err(|e| e.to_string())
    }

    async fn session_synthetic(&self, m: SyntheticMessage) -> Result<(), String> {
        self.app
            .sessions
            .synthetic(&m.session_id, m.id, m.text, Some(m.description), Some(m.metadata), None)
            .await
            .map_err(|e| e.to_string())?;
        if let Some(c) = self.app.coordinator.get() {
            c.wake(&m.session_id, crate::inbox::Scope::Input);
        }
        Ok(())
    }

    async fn job_start(&self, start: JobStart) -> Result<JobInfo, String> {
        Ok(self.app.jobs.start(start.id, start.kind, start.title, start.recovery, start.run))
    }

    async fn job_block(&self, id: &str, session_id: &str) -> Result<Option<JobBlockResult>, String> {
        Ok(self.app.jobs.block(id, session_id).await)
    }

    async fn job_background(&self, id: &str) -> Result<Option<JobInfo>, String> {
        Ok(self.app.jobs.background(id))
    }

    async fn job_wait(&self, id: &str) -> Result<Option<JobInfo>, String> {
        Ok(self.app.jobs.wait(id).await)
    }

    async fn job_cancel(&self, id: &str) -> Result<(), String> {
        self.app.jobs.cancel(id);
        Ok(())
    }

    async fn job_complete_background(&self, notification_id: &str) -> Result<(), String> {
        self.app.jobs.complete_background(notification_id);
        Ok(())
    }

    async fn subagent_job_start(&self, recovery: JobRecovery) -> Result<(), String> {
        let JobRecovery::Subagent { child_session_id, description, .. } = &recovery else { return Err("not a subagent".into()) };
        let app = self.app.clone();
        let child = child_session_id.clone();
        let run = Box::pin(async move {
            if let Some(c) = app.coordinator.get() {
                if let crate::coordinator::Exit::Failure(e) = c.run(&child).await {
                    return Err(e.to_string());
                }
            }
            let msgs = app.db.session_messages(&child).map_err(|e| e.to_string())?;
            let last = msgs.iter().rev().take(20).map(|(_, m)| m).find(|m| {
                matches!(m, oc_schema::Message::Assistant(a) if a.time.completed.is_some() && a.error.is_none())
            });
            Ok(crate::job::subagent_text(last))
        });
        self.app.jobs.start(child_session_id.clone(), "subagent".into(), Some(description.clone()), Some(recovery.clone()), run);
        Ok(())
    }

    async fn subagent_job_background(&self, recovery: JobRecovery) -> Result<(), String> {
        let JobRecovery::Subagent { child_session_id, .. } = &recovery else { return Ok(()) };
        if let Some(info) = self.app.jobs.background(child_session_id) {
            self.subagent_job_notify(recovery.clone(), info.started_at).await?;
        }
        Ok(())
    }

    async fn subagent_job_notify(&self, recovery: JobRecovery, _started_at: u64) -> Result<(), String> {
        let JobRecovery::Subagent { child_session_id, .. } = &recovery else { return Ok(()) };
        let app = self.app.clone();
        let child = child_session_id.clone();
        tokio::spawn(async move {
            let Some(info) = app.jobs.wait(&child).await else { return };
            crate::job::deliver_subagent(&app, info.status, info.output, info.error, info.notification_id, &recovery, true).await;
        });
        Ok(())
    }

    async fn session_environment(&self, session_id: &str) -> Option<std::collections::HashMap<String, String>> {
        self.app.environments.lock().get(session_id).cloned()
    }

    async fn shell_create_before(&self, invocation: &mut ShellInvocation) -> Result<(), String> {
        match &self.location {
            Some(key) => self.app.plugins.shell_before(key, invocation).await,
            None => Ok(()),
        }
    }

    async fn worktree_create(&self, project_id: &str, name: &str) -> Result<Value, String> {
        let dir = crate::worktree::create(&self.app.bus, &self.app.global, project_id, None, None, None, Some(name), None)
            .await
            .map_err(|e| e.to_string())?;
        Ok(serde_json::json!({"directory": dir.display().to_string()}))
    }

    async fn worktree_list(&self, project_id: &str) -> Result<Value, String> {
        crate::worktree::list(&self.app.bus, project_id).map(Value::Array).map_err(|e| e.to_string())
    }

    async fn worktree_remove(&self, project_id: &str, directory: &str, force: bool) -> Result<(), WorktreeRemoveError> {
        crate::worktree::remove(&self.app.bus, project_id, directory, force).await.map_err(|e| match e {
            crate::worktree::WorktreeError::InvalidDirectory(_) => WorktreeRemoveError::InvalidDirectory,
            crate::worktree::WorktreeError::Git { force_required: true, .. } => WorktreeRemoveError::ForceRequired,
            other => WorktreeRemoveError::Other(other.to_string()),
        })
    }

    async fn agent_resolve(&self, name: &str) -> Result<Option<AgentSummary>, String> {
        let agents = self.app.agents.read();
        Ok(agents.items.get(name).map(|a| AgentSummary {
            id: a.id.clone(),
            name: a.name.clone(),
            mode: serde_json::to_value(a.mode).unwrap().as_str().unwrap_or("primary").to_string(),
            hidden: a.hidden,
            description: a.description.clone(),
            model: a.model.clone(),
        }))
    }
}
