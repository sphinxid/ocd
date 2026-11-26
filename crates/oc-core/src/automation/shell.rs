//! Shell actions (`core/src/automation/shell.ts`, 2.4.1 #65186/#65217/#65269): `{ type: "shell", command }` runs a
//! fixed command on each trigger with no model.
//!
//! The command is approved once at creation, while the user is present, with the same requests the shell tool would
//! make for it (`ShellPermissions.requests` with `once`: nothing is saved as a pattern, and a command whose
//! subcommands the scanner finds no resource for is asked as itself). Runs never recheck permissions; pause or
//! delete the automation to stop them. A run starts the command with no timeout and advances the automation as soon
//! as it launches, so the next slot or event can start before it exits; when it exits, its exit code and output tail
//! become the automation's last output (`Exit code: N\n...`), unless a newer run or a pause got there first.

use std::path::Path;
use std::sync::Arc;

use serde_json::{Map, Value, json};

use oc_schema::types::{Effect, SessionInfo};
use oc_tools::file_access::{self, Kind};
use oc_tools::host::ShellInvocation;
use oc_tools::shell::{parse, select};

use super::{AutomationError, Automations, Invocation, matches_author};
use crate::permission::AssertInput;

/// One permission the command needs (`ShellPermissions.requests`).
#[derive(Debug, Clone, PartialEq)]
pub struct Request {
    pub action: String,
    pub resources: Vec<String>,
}

/// `ShellPermissions.requests(access, { command, program, target, once: true, rules })` for a command run in
/// `directory` (inside the session's location, so internal): the external directories it reaches, then the command.
/// A command the scanner can't analyze fails while any shell deny rule is configured (its subcommands can't be checked
/// against it), and otherwise asks for every directory and the whole command.
pub fn requests(
    command: &str,
    program: &str,
    directory: &Path,
    worktree: &Path,
    home: &Path,
    rules: &[oc_schema::types::PermissionRule],
) -> Result<Vec<Request>, String> {
    let target = file_access::resolve_in(directory, worktree, home, ".", Some(Kind::Directory));
    let cwd = target.absolute.to_string_lossy().into_owned();
    let parsed = match parse::scan(command, program, &cwd, home) {
        Ok(parsed) => parsed,
        Err(opaque) => {
            if rules.iter().any(|rule| rule.effect == Effect::Deny && crate::wildcard::matches("shell", &rule.action)) {
                return Err(oc_tools::tools::shell::unverifiable_message(&opaque.reason));
            }
            return Ok(vec![
                Request { action: "external_directory".into(), resources: vec!["*".into()] },
                Request { action: "shell".into(), resources: vec![command.to_string()] },
            ]);
        }
    };
    // Directory arguments are relative to the command's working directory.
    let directories: Vec<file_access::Target> = parsed
        .directories
        .iter()
        .map(|dir| {
            let absolute = file_access::resolve_path(&target.absolute, dir, home);
            file_access::resolve_in(directory, worktree, home, &absolute.to_string_lossy(), Some(Kind::Directory))
        })
        .collect();
    let mut out = Vec::new();
    let mut external: Vec<String> = Vec::new();
    for item in std::iter::once(&target).chain(directories.iter()) {
        if let Some(dir) = &item.external_directory
            && !external.contains(&dir.resource)
        {
            external.push(dir.resource.clone());
        }
    }
    if !external.is_empty() {
        out.push(Request { action: "external_directory".into(), resources: external });
    }
    let resources =
        if parsed.commands.is_empty() { vec![command.to_string()] } else { parsed.commands.iter().map(|c| c.resource.clone()).collect() };
    out.push(Request { action: "shell".into(), resources });
    Ok(out)
}

/// `AutomationShell.authorize`: in the creating session's location, asks for the command's permissions without saving
/// them, so only the command stored in this automation is approved.
pub async fn authorize(
    automations: &Automations,
    owner: &SessionInfo,
    name: &str,
    trigger: &Value,
    command: &str,
    invocation: Option<&Invocation>,
) -> Result<(), AutomationError> {
    let directory = owner.location.directory.clone();
    let env = automations.env(owner, &directory).await?;
    if let Some(invocation) = invocation
        && !matches_author(owner, env.agent.as_ref().map(|a| a.id.as_str()), invocation)
    {
        return Err(AutomationError::Invalid("Create the automation with the session's current agent".into()));
    }
    let program = select::resolve(select::Priority::Compat, env.shell.as_deref(), Some(&env.bin));
    let agent_rules = env.agent.as_ref().map(|a| a.permissions.clone()).unwrap_or_default();
    let rules = crate::permission::merge(&[&agent_rules, owner.permissions.as_deref().unwrap_or(&[])]);
    let requests = requests(command, &program, &env.location.directory, &env.location.project.canonical, &env.home, &rules)
        .map_err(AutomationError::Invalid)?;
    let app = automations.app().ok_or(AutomationError::Declined)?;
    let mut metadata = Map::new();
    metadata.insert("automation".into(), json!(name));
    metadata.insert("trigger".into(), trigger.clone());
    metadata.insert("unattended".into(), json!(true));
    metadata.insert("command".into(), json!(command));
    for request in requests {
        // No save patterns: approval applies only to the command stored in this automation.
        let input = AssertInput {
            id: None,
            session_id: invocation.map(|i| i.session_id.clone()).unwrap_or_else(|| owner.id.clone()),
            action: request.action,
            resources: request.resources,
            save: None,
            metadata: Some(metadata.clone()),
            source: invocation.map(|i| i.source.clone()),
            agent: Some(invocation.map(|i| i.agent.clone()).unwrap_or_else(|| env.agent_id.clone())),
        };
        app.permissions.assert(input).await.map_err(AutomationError::from_permission)?;
    }
    Ok(())
}

/// A started command (`AutomationShell.start`'s result): waits for its exit.
pub struct Started {
    service: Arc<oc_tools::shell::service::ShellService>,
    info: oc_schema::message::ShellInfo,
    max_lines: usize,
    max_bytes: usize,
}

/// Stops the command if its wait is dropped before it ends (the server shutting down).
struct StopOnDrop {
    service: Arc<oc_tools::shell::service::ShellService>,
    id: String,
    armed: bool,
}

impl Drop for StopOnDrop {
    fn drop(&mut self) {
        if self.armed {
            let _ = self.service.stop(&self.id, oc_schema::message::ShellStatus::Killed);
        }
    }
}

impl Started {
    /// `Exit code: N\n<output tail>`, and whether it exited with 0.
    pub async fn wait(self) -> (String, bool) {
        let mut guard = StopOnDrop { service: self.service.clone(), id: self.info.id.clone(), armed: true };
        let result = self.service.result(&self.info, self.max_lines, self.max_bytes).await;
        guard.armed = false;
        let captured = oc_tools::shell::result::output(&result);
        let exit = captured.exit.map(|e| e.to_string()).unwrap_or_else(|| "unknown".into());
        (format!("Exit code: {exit}\n{}", captured.output), captured.exit == Some(0))
    }
}

/// `AutomationShell.start`: starts the stored command in `directory`, under the session's environment and the
/// `shell.create.before` hooks, which can set the environment while the command, its working directory and shell
/// stay fixed, with no timeout. Approved once at creation; runs intentionally don't recheck permissions.
pub async fn start(automations: &Automations, owner: &SessionInfo, project_id: &str, command: &str, directory: &str) -> Result<Started, AutomationError> {
    let env = automations.env(owner, directory).await?;
    let engine = automations.engine().ok_or_else(|| AutomationError::Other("Session runner is not available".into()))?;
    let app = automations.app().ok_or_else(|| AutomationError::Other("The server is shutting down".into()))?;
    let program = select::resolve(select::Priority::Compat, env.shell.as_deref(), Some(&env.bin));
    let mut vars: std::collections::HashMap<String, String> =
        app.environments.lock().get(&owner.id).cloned().unwrap_or_else(|| std::env::vars().collect());
    vars.insert("TERM".into(), "xterm-256color".into());
    vars.insert("OPENCODE_TERMINAL".into(), "1".into());
    let mut invocation = ShellInvocation { command: command.to_string(), cwd: directory.to_string(), timeout: 0, shell: program.clone(), env: vars };
    app.plugins.shell_before(&env.location.key, &mut invocation).await.map_err(AutomationError::Other)?;
    // Hooks can set the environment, but the stored command and its working directory are fixed.
    invocation.command = command.to_string();
    invocation.cwd = directory.to_string();
    invocation.shell = program;
    invocation.timeout = 0;
    let mut metadata = Map::new();
    metadata.insert("sessionID".into(), json!(owner.id));
    let placement = oc_tools::shell::daemon::Placement {
        project_id: project_id.to_string(),
        directory: directory.to_string(),
        workspace_id: None,
        tool_call: None,
        waited: false,
    };
    let info = engine.shell.create(&invocation, metadata, &placement).await.map_err(AutomationError::Other)?;
    let entries = &env.location.entries;
    let max_lines = oc_config::fold::tool_output(entries).and_then(|t| t.max_lines).unwrap_or(2000) as usize;
    let max_bytes = oc_config::fold::tool_output(entries).and_then(|t| t.max_bytes).unwrap_or(51200) as usize;
    Ok(Started { service: engine.shell.clone(), info, max_lines, max_bytes })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    #[test]
    fn asks_for_the_command_and_external_directories_once() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().join("home");
        let project = dir.path().join("project");
        std::fs::create_dir_all(&project).unwrap();
        let outside = dir.path().join("outside");
        std::fs::create_dir_all(&outside).unwrap();
        let found = requests("git status", "/bin/sh", &project, &project, &home, &[]).unwrap();
        assert_eq!(found.iter().map(|r| r.action.as_str()).collect::<Vec<_>>(), vec!["shell"]);
        assert_eq!(found[0].resources, vec!["git status"]);
        let command = format!("cd {} && ls", outside.display());
        let found = requests(&command, "/bin/sh", &project, &project, &home, &[]).unwrap();
        assert_eq!(found[0].action, "external_directory");
        assert_eq!(found[1].action, "shell");
        // An opaque command asks for every directory and itself, unless a shell deny rule is configured.
        let opaque = "alias p=true; printf hello > marker";
        let found = requests(opaque, "/bin/sh", &project, &project, &home, &[]).unwrap();
        assert_eq!(found, vec![
            Request { action: "external_directory".into(), resources: vec!["*".into()] },
            Request { action: "shell".into(), resources: vec![opaque.into()] },
        ]);
        let deny = oc_schema::types::PermissionRule { action: "shell".into(), resource: "rm *".into(), effect: Effect::Deny };
        assert!(requests(opaque, "/bin/sh", &project, &project, &home, &[deny]).unwrap_err().starts_with("Portable shell scanner cannot analyze command"));
    }
}
