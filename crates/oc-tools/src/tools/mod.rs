//! Built-in tool implementations (`core/src/tool/plugin/*`).

pub mod edit;
pub mod glob;
pub mod grep;
pub mod mcp;
pub mod mcp_resource;
pub mod opencode;
pub mod patch;
pub mod question;
pub mod read;
pub mod shell;
pub mod skill;
pub mod subagent;
pub mod webfetch;
pub mod websearch;
pub mod write;

use std::sync::Arc;

use serde_json::Value;

use crate::formatter::{FormatEnv, Formatter};
use crate::host::PermissionError;
use crate::tool::{ToolContext, ToolDefinition, ToolError};

/// Load an embedded schema file (`schemas/<name>.json`): `(input, output)`.
pub(crate) fn schemas(text: &str) -> (Value, Option<Value>) {
    crate::tool::schema_json(text)
}

pub(crate) fn definition(name: &str, description: impl Into<String>, schema_text: &str) -> ToolDefinition {
    let (input, output) = schemas(schema_text);
    ToolDefinition::new(name, description, input).with_output(output)
}

/// Assert a permission; failures become the leaf's error (declines tunnel through).
pub(crate) async fn assert(
    ctx: &ToolContext,
    action: &str,
    resources: Vec<String>,
    save: Option<Vec<String>>,
    metadata: Option<oc_schema::Metadata>,
) -> Result<(), PermissionError> {
    ctx.host.ask_permission(ctx.permission(action, resources, save, metadata)).await
}

pub(crate) fn perm(leaf: impl Into<String>) -> impl FnOnce(PermissionError) -> ToolError {
    let leaf = leaf.into();
    move |error| error.into_tool_error(leaf)
}

/// Run the configured formatters for `path`.
pub(crate) async fn format_file(formatter: &Formatter, ctx: &ToolContext, path: &std::path::Path) -> bool {
    let env = FormatEnv {
        directory: &ctx.directory,
        worktree: &ctx.worktree,
        bin: &ctx.global.bin,
        config: &ctx.config.formatter,
        host: &ctx.host,
    };
    formatter.file(path, &env).await
}

/// Services shared by the built-in tools of one Location.
#[derive(Clone)]
pub struct Services {
    pub formatter: Arc<Formatter>,
    pub ripgrep: Arc<crate::ripgrep::Ripgrep>,
    pub shell: Arc<crate::shell::service::ShellService>,
    pub websearch: Arc<crate::websearch::WebSearch>,
    pub http: reqwest::Client,
}

/// Built-in tools in upstream registration order (`core/src/plugin/internal.ts`):
/// patch, edit, glob, grep, opencode.*, MCP resource tools, question, read, shell, skill,
/// subagent, webfetch, websearch, write.
pub fn builtin(services: &Services) -> (Vec<Arc<dyn crate::tool::Tool>>, Vec<crate::tool::Namespace>) {
    let mut tools: Vec<Arc<dyn crate::tool::Tool>> = vec![
        Arc::new(patch::PatchTool::new(services.formatter.clone())),
        Arc::new(edit::EditTool::new(services.formatter.clone())),
        Arc::new(glob::GlobTool::new(services.ripgrep.clone())),
        Arc::new(grep::GrepTool::new(services.ripgrep.clone())),
    ];
    tools.extend(opencode::tools());
    tools.extend(mcp_resource::tools());
    tools.push(Arc::new(question::QuestionTool));
    tools.push(Arc::new(read::ReadTool));
    tools.push(Arc::new(shell::ShellTool::new(services.shell.clone())));
    tools.push(Arc::new(skill::SkillTool));
    tools.push(Arc::new(subagent::SubagentTool));
    tools.push(Arc::new(webfetch::WebFetchTool::new(services.http.clone())));
    tools.push(Arc::new(websearch::WebSearchTool::new(services.websearch.clone())));
    tools.push(Arc::new(write::WriteTool::new(services.formatter.clone())));
    (tools, vec![opencode::namespace()])
}

/// Register all built-ins into a registry.
pub fn register_builtins(registry: &mut crate::registry::Registry, services: &Services) {
    let (tools, namespaces) = builtin(services);
    for namespace in namespaces {
        let _ = registry.namespace(namespace);
    }
    for tool in tools {
        let _ = registry.add(tool);
    }
}
