//! `opencode` namespace tools (CodeMode; `core/src/tool/plugin/opencode.ts`).

use std::sync::Arc;

use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::file_access::{self, Kind};
use crate::host::{ModelSummary, WorktreeRemoveError};
use crate::tool::{DefinitionContext, ErrorCause, Namespace, Tool, ToolContext, ToolDefinition, ToolError, ToolOptions, ToolResult, decode};
use crate::util::js;
use crate::validate::{Issue, PathSegment};

pub const NAMESPACE_DESCRIPTION: &str = "Tools for managing OpenCode itself, such as working with sessions and worktrees, searching the available models, and reading MCP resources.";

/// System part pushed on every request by the opencode tools plugin.
pub const SYSTEM_HINT: &str = "When you create a worktree outside the current working directory and intend to use it as your primary working directory, consider using `execute` to call `tools.opencode.session_move` and make the worktree the session's working directory.";

pub fn namespace() -> Namespace {
    Namespace { name: "opencode".into(), description: NAMESPACE_DESCRIPTION.into() }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind2 {
    SessionRename,
    SessionMove,
    WorktreeCreate,
    WorktreeList,
    WorktreeRemove,
    Models,
}

pub struct OpenCodeTool {
    kind: Kind2,
}

pub fn tools() -> Vec<Arc<dyn Tool>> {
    [Kind2::SessionRename, Kind2::SessionMove, Kind2::WorktreeCreate, Kind2::WorktreeList, Kind2::WorktreeRemove, Kind2::Models]
        .into_iter()
        .map(|kind| Arc::new(OpenCodeTool { kind }) as Arc<dyn Tool>)
        .collect()
}

impl OpenCodeTool {
    fn info(&self) -> (&'static str, &'static str, &'static str, bool) {
        match self.kind {
            Kind2::SessionRename => (
                "session_rename",
                "Rename a session, or omit sessionID to rename the current session. Use a short, specific title that summarizes the work being done.",
                include_str!("../../schemas/opencode_session_rename.json"),
                false,
            ),
            Kind2::SessionMove => (
                "session_move",
                "Move a session to another directory, or omit sessionID to move the current session. The current session moves at the next safe boundary; do not run destination-dependent tools in the same execute call.",
                include_str!("../../schemas/opencode_session_move.json"),
                true,
            ),
            Kind2::WorktreeCreate => (
                "worktree_create",
                "Create a named worktree. Returns its directory.",
                include_str!("../../schemas/opencode_worktree_create.json"),
                true,
            ),
            Kind2::WorktreeList => {
                ("worktree_list", "List a repository's worktrees.", include_str!("../../schemas/opencode_worktree_list.json"), true)
            }
            Kind2::WorktreeRemove => (
                "worktree_remove",
                "Remove a worktree. The repository location selects configuration; directory identifies the worktree to remove.",
                include_str!("../../schemas/opencode_worktree_remove.json"),
                true,
            ),
            Kind2::Models => (
                "models",
                "Search the models available to use. Use this to turn a model name the user mentions into an exact reference for a tool that takes a model. Check your own provider first.",
                include_str!("../../schemas/opencode_models.json"),
                false,
            ),
        }
    }
}

/// `removeFailure(directory, error)`.
pub fn remove_failure(directory: &str, error: &WorktreeRemoveError) -> String {
    match error {
        WorktreeRemoveError::DirectoryUnavailable => {
            format!("Unable to remove worktree {directory}: the directory does not exist or is not a directory.")
        }
        WorktreeRemoveError::InvalidDirectory => format!(
            "Unable to remove {directory}: it is not a worktree of this project. Use worktree_list to see the project's worktrees."
        ),
        WorktreeRemoveError::StrategyUnavailable { strategy } => {
            format!("Unable to remove worktree {directory}: its worktree strategy {strategy} is not available.")
        }
        WorktreeRemoveError::ForceRequired => format!("Unable to remove worktree {directory}: it has modified or untracked files."),
        WorktreeRemoveError::Other(_) => format!("Unable to remove worktree {directory}."),
    }
}

/// `models` filtering/sorting/paging.
pub fn models_page(
    models: Vec<ModelSummary>,
    names: &[(String, String)],
    own: Option<&str>,
    query: Option<&str>,
    provider: Option<&str>,
    all: bool,
    offset: usize,
    limit: usize,
) -> Value {
    let name_of = |id: &str| names.iter().find(|(p, _)| p == id).map(|(_, n)| n.clone());
    let terms: Vec<String> = query.map(|q| q.to_lowercase().split_whitespace().map(String::from).collect()).unwrap_or_default();
    let provider = provider.map(str::to_lowercase);
    let mut matching: Vec<ModelSummary> = models
        .into_iter()
        .filter(|m| {
            provider.as_ref().is_none_or(|p| {
                m.provider_id.to_lowercase() == *p || name_of(&m.provider_id).is_some_and(|n| n.to_lowercase() == *p)
            })
        })
        .filter(|m| {
            let text = format!("{}/{} {}", m.provider_id, m.id, m.name).to_lowercase();
            terms.iter().all(|t| text.contains(t))
        })
        .collect();
    matching.sort_by(|l, r| {
        let lo = own == Some(l.provider_id.as_str());
        let ro = own == Some(r.provider_id.as_str());
        ro.cmp(&lo)
            .then_with(|| js::locale_compare(&l.provider_id, &r.provider_id))
            .then_with(|| r.released.partial_cmp(&l.released).unwrap_or(std::cmp::Ordering::Equal))
    });
    if !all {
        let mut seen: Vec<(String, String)> = Vec::new();
        matching.retain(|m| match &m.family {
            None => true,
            Some(f) => {
                let key = (m.provider_id.clone(), f.clone());
                if seen.contains(&key) {
                    false
                } else {
                    seen.push(key);
                    true
                }
            }
        });
    }
    let total = matching.len();
    let page: Vec<&ModelSummary> = matching.iter().skip(offset).take(limit).collect();
    let mut provider_ids: Vec<String> = Vec::new();
    for m in &page {
        if !provider_ids.contains(&m.provider_id) {
            provider_ids.push(m.provider_id.clone());
        }
    }
    let providers: Vec<Value> = provider_ids
        .iter()
        .map(|id| {
            json!({
                "id": id,
                "name": name_of(id).unwrap_or_else(|| id.clone()),
                "models": page.iter().filter(|m| &m.provider_id == id).map(|m| json!({
                    "id": format!("{}/{}", m.provider_id, m.id),
                    "name": m.name,
                    "released": m.released,
                    "variants": m.variants,
                    "cost": m.cost,
                    "status": m.status,
                })).collect::<Vec<_>>(),
            })
        })
        .collect();
    json!({
        "providers": providers,
        "total": total,
        "next": if offset + limit < total { json!(offset + limit) } else { Value::Null },
    })
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Args {
    #[serde(rename = "sessionID")]
    session_id: Option<String>,
    title: Option<String>,
    directory: Option<String>,
    name: Option<String>,
    force: Option<bool>,
    query: Option<String>,
    provider: Option<String>,
    #[serde(rename = "type")]
    kind: Option<String>,
    all: Option<bool>,
    limit: Option<usize>,
    offset: Option<usize>,
}

fn err(message: String, cause: String) -> ToolError {
    ToolError::with_cause(message, ErrorCause::Error(cause))
}

#[async_trait]
impl Tool for OpenCodeTool {
    fn name(&self) -> &str {
        self.info().0
    }

    fn options(&self) -> ToolOptions {
        let (_, _, _, pinned) = self.info();
        ToolOptions {
            namespace: Some("opencode".into()),
            permission: (self.kind == Kind2::WorktreeRemove).then(|| "edit".into()),
            codemode: true,
            pinned,
        }
    }

    fn definition(&self, _ctx: &DefinitionContext) -> ToolDefinition {
        let (name, description, schema, _) = self.info();
        super::definition(&format!("opencode_{name}"), description, schema)
    }

    fn validate(&self, input: &Value) -> Vec<Issue> {
        let schema = self.definition(&DefinitionContext::default()).input_schema;
        let mut issues = crate::validate::validate(&schema, input, crate::validate::Flavor::Effect);
        if self.kind == Kind2::WorktreeCreate && issues.is_empty() {
            if let Some(name) = input.get("name").and_then(Value::as_str) {
                if js::trim(name).is_empty() {
                    issues.push(Issue::new(vec![PathSegment::Key("name".into())], "Expected a value with a length of at least 1"));
                }
            }
        }
        issues
    }

    async fn execute(&self, input: Value, ctx: ToolContext) -> Result<ToolResult, ToolError> {
        let args: Args = decode(input)?;
        let host = &ctx.host;
        match self.kind {
            Kind2::SessionRename => {
                let session = args.session_id.unwrap_or_else(|| ctx.session_id.clone());
                let title = js::trim(args.title.as_deref().unwrap_or("")).to_string();
                if title.is_empty() {
                    return Err(ToolError::new("Session title must not be empty"));
                }
                host.session_rename(&session, &title).await.map_err(|m| err(format!("Unable to rename session {session}"), m))?;
                Ok(ToolResult::text(format!("Renamed session {session} to {title}.")).with_output(json!({"sessionID": session, "title": title})))
            }
            Kind2::SessionMove => {
                let session = args.session_id.unwrap_or_else(|| ctx.session_id.clone());
                let directory = args.directory.unwrap_or_default();
                host.session_move(&session, &directory).await.map_err(|m| err(format!("Unable to move session to {directory}"), m))?;
                Ok(ToolResult::text(format!("Moved session {session} to {directory}.")).with_output(json!({"sessionID": session, "directory": directory})))
            }
            Kind2::WorktreeCreate => {
                let name = js::trim(args.name.as_deref().unwrap_or("")).to_string();
                let output = host.worktree_create(&ctx.project_id, &name).await.map_err(|m| err(format!("Unable to create worktree {name}"), m))?;
                let directory = output.get("directory").and_then(Value::as_str).unwrap_or("").to_string();
                Ok(ToolResult::text(format!("Created worktree in {directory}.")).with_output(output))
            }
            Kind2::WorktreeList => {
                let worktrees = host.worktree_list(&ctx.project_id).await.map_err(|m| err("Unable to list worktrees".into(), m))?;
                Ok(ToolResult { output: Some(json!({"worktrees": worktrees})), content: Vec::new(), metadata: None })
            }
            Kind2::WorktreeRemove => {
                let directory = args.directory.unwrap_or_default();
                let leaf = format!("Unable to remove worktree {directory}");
                let target = file_access::resolve(&ctx, &directory, Some(Kind::Directory));
                super::assert(&ctx, "edit", vec![target.resource.clone()], Some(vec!["*".into()]), None).await.map_err(super::perm(leaf))?;
                let absolute = target.absolute.to_string_lossy().into_owned();
                host.worktree_remove(&ctx.project_id, &absolute, args.force.unwrap_or(false))
                    .await
                    .map_err(|e| ToolError::new(remove_failure(&absolute, &e)))?;
                Ok(ToolResult::text(format!("Removed worktree {absolute}.")).with_output(json!({"directory": absolute, "removed": true})))
            }
            Kind2::Models => {
                let fail = |m: String| err("Unable to list models".into(), m);
                let own = host.session_get(&ctx.session_id).await.map_err(fail)?.model.map(|m| m.provider_id);
                let names: Vec<(String, String)> = host.providers_list().await.map_err(fail)?.into_iter().map(|p| (p.id, p.name)).collect();
                let kind = args.kind.clone().unwrap_or_else(|| "chat".into());
                let models = host.models_list(&[kind]).await.map_err(fail)?;
                let output = models_page(
                    models,
                    &names,
                    own.as_deref(),
                    args.query.as_deref(),
                    args.provider.as_deref(),
                    args.all == Some(true),
                    args.offset.unwrap_or(0),
                    args.limit.unwrap_or(20),
                );
                Ok(ToolResult { output: Some(output), content: Vec::new(), metadata: None })
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn model(p: &str, id: &str, family: Option<&str>, released: f64) -> ModelSummary {
        ModelSummary {
            provider_id: p.into(),
            id: id.into(),
            name: id.to_uppercase(),
            family: family.map(String::from),
            released,
            variants: vec![],
            cost: json!([]),
            status: "active".into(),
        }
    }

    #[test]
    fn definitions_and_names() {
        let tools = tools();
        let names: Vec<String> = tools.iter().map(|t| t.definition(&DefinitionContext::default()).name).collect();
        assert_eq!(names[0], "opencode_session_rename");
        assert_eq!(tools[4].options().permission.as_deref(), Some("edit"));
        assert!(tools[1].options().pinned);
        assert!(!tools[5].options().pinned);
    }

    #[test]
    fn worktree_create_trims() {
        let tool = OpenCodeTool { kind: Kind2::WorktreeCreate };
        let issues = tool.validate(&json!({"name": "  "}));
        assert_eq!(issues[0].message, "Expected a value with a length of at least 1");
        assert!(tool.validate(&json!({"name": " a "})).is_empty());
    }

    #[test]
    fn models_listing() {
        let models = vec![
            model("openai", "gpt-5", Some("gpt"), 3.0),
            model("openai", "gpt-4", Some("gpt"), 1.0),
            model("anthropic", "claude", Some("claude"), 2.0),
            model("zed", "z1", None, 0.0),
        ];
        let names = vec![("openai".to_string(), "OpenAI".to_string())];
        let out = models_page(models.clone(), &names, Some("openai"), None, None, false, 0, 20);
        assert_eq!(out["total"], json!(3));
        assert_eq!(out["providers"][0]["id"], json!("openai"));
        assert_eq!(out["providers"][0]["name"], json!("OpenAI"));
        assert_eq!(out["providers"][0]["models"][0]["id"], json!("openai/gpt-5"));
        assert_eq!(out["providers"][1]["id"], json!("anthropic"));
        assert_eq!(out["next"], Value::Null);
        let out = models_page(models.clone(), &names, None, Some("GPT"), Some("openai"), true, 0, 1);
        assert_eq!(out["total"], json!(2));
        assert_eq!(out["next"], json!(1));
    }

    #[test]
    fn remove_messages() {
        assert_eq!(remove_failure("/w", &WorktreeRemoveError::ForceRequired), "Unable to remove worktree /w: it has modified or untracked files.");
        assert_eq!(remove_failure("/w", &WorktreeRemoveError::Other("x".into())), "Unable to remove worktree /w.");
    }
}
