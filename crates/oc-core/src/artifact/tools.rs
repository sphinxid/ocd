//! The `opencode.tool.artifact` built-in plugin (`core/src/tool/plugin/artifact.ts`): `artifact_create`,
//! `artifact_read`, `artifact_update` and `artifact_delete` in the `opencode` namespace, registered only
//! where `experimental.artifacts` is on. `artifact_create`'s description names the providers.

use std::sync::{Arc, LazyLock};

use async_trait::async_trait;
use serde_json::{Value, json};

use oc_tools::registry::Registry;
use oc_tools::tool::{DefinitionContext, ErrorCause, Tool, ToolContext, ToolDefinition, ToolError, ToolOptions, ToolResult};

use super::{CreateInput, Error, Service, UpdateInput};
use crate::app::App;
use crate::location::Location;

pub const PLUGIN_ID: &str = "opencode.tool.artifact";

/// The tools' JSON Schemas as upstream's `definition` emits them (`scripts/artifact-schemas.ts`).
static SCHEMAS: LazyLock<Vec<(String, Value, Value)>> = LazyLock::new(|| {
    let list: Vec<Value> = serde_json::from_str(include_str!("../../../../assets/artifact/tools.json")).expect("artifact tool schemas");
    list.into_iter()
        .map(|v| {
            (
                v["name"].as_str().unwrap_or_default().to_string(),
                v.get("inputSchema").cloned().unwrap_or(Value::Null),
                v.get("outputSchema").cloned().unwrap_or(Value::Null),
            )
        })
        .collect()
});

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    Create,
    Read,
    Update,
    Delete,
}

impl Kind {
    fn name(self) -> &'static str {
        match self {
            Kind::Create => "artifact_create",
            Kind::Read => "artifact_read",
            Kind::Update => "artifact_update",
            Kind::Delete => "artifact_delete",
        }
    }
}

pub struct ArtifactTool {
    kind: Kind,
    app: Arc<App>,
    location: Arc<Location>,
    /// `artifact_create`'s description (`renderCreateDescription`), fixed when the registry was built.
    description: String,
}

/// Whether `name` (un-namespaced) is one of this plugin's tools.
pub fn is_artifact_tool(name: &str) -> bool {
    matches!(name, "artifact_create" | "artifact_read" | "artifact_update" | "artifact_delete")
}

/// The four tools of a location where artifacts are on.
pub async fn tools(app: &Arc<App>, location: &Arc<Location>) -> Vec<Arc<dyn Tool>> {
    if !super::enabled(location) {
        return vec![];
    }
    let providers = super::providers(&app.plugins, &location.key).await;
    let description = super::create_description(&providers);
    [Kind::Create, Kind::Read, Kind::Update, Kind::Delete]
        .into_iter()
        .map(|kind| {
            Arc::new(ArtifactTool { kind, app: app.clone(), location: location.clone(), description: description.clone() })
                as Arc<dyn Tool>
        })
        .collect()
}

/// Inserts the tools where the plugin registers them: after the tools of the built-in plugins before
/// it (`opencode.tool.read`), before those after it.
pub fn insert(registry: &mut Registry, tools: Vec<Arc<dyn Tool>>) {
    let rank = |id: &str| crate::plugins::BUILTIN_PLUGINS.iter().position(|p| *p == id);
    let own = rank(PLUGIN_ID).unwrap_or(usize::MAX);
    let position = registry
        .list()
        .iter()
        .position(|(_, tool)| crate::plugins::tool_plugin(tool.as_ref()).and_then(rank).is_none_or(|r| r > own))
        .unwrap_or(registry.list().len());
    for (offset, tool) in tools.into_iter().enumerate() {
        let _ = registry.insert(position + offset, tool);
    }
}

fn failure(error: Error) -> ToolError {
    match error {
        Error::Defect(message) => ToolError::new(message),
        error => {
            let message = error.message();
            ToolError::with_cause(message.clone(), ErrorCause::Error(message))
        }
    }
}

fn opt_string(input: &Value, key: &str) -> Option<String> {
    input.get(key).and_then(Value::as_str).map(str::to_string)
}

#[async_trait]
impl Tool for ArtifactTool {
    fn name(&self) -> &str {
        self.kind.name()
    }

    fn options(&self) -> ToolOptions {
        ToolOptions { namespace: Some("opencode".into()), permission: None, codemode: true, pinned: false }
    }

    fn definition(&self, _ctx: &DefinitionContext) -> ToolDefinition {
        let description = match self.kind {
            Kind::Create => self.description.clone(),
            Kind::Read => "Read an artifact's metadata and file contents so you can inspect or update it.".into(),
            Kind::Update => "Replace an existing artifact's files at the same ID and URL.".into(),
            Kind::Delete => "Delete an artifact and remove its stored copy.".into(),
        };
        let (input, output) = SCHEMAS
            .iter()
            .find(|(name, _, _)| name == self.kind.name())
            .map(|(_, i, o)| (i.clone(), o.clone()))
            .unwrap_or((Value::Null, Value::Null));
        ToolDefinition::new(format!("opencode_{}", self.kind.name()), description, input).with_output(Some(output))
    }

    async fn execute(&self, input: Value, ctx: ToolContext) -> Result<ToolResult, ToolError> {
        let service = Service::new(&self.app, &self.location);
        let files = super::string_entries(input.get("files").unwrap_or(&Value::Null));
        match self.kind {
            Kind::Create => {
                let info = service
                    .create(CreateInput {
                        title: opt_string(&input, "title").unwrap_or_default(),
                        files,
                        entry: opt_string(&input, "entry"),
                        provider: opt_string(&input, "provider"),
                        session_id: Some(ctx.session_id.clone()),
                    })
                    .await
                    .map_err(failure)?;
                let content = format!("Published {} ({} via {}): {}", info.title, info.id, info.provider, info.url);
                Ok(ToolResult::text(content).with_output(serde_json::to_value(&info).unwrap_or(Value::Null)))
            }
            Kind::Read => {
                let id = opt_string(&input, "id").unwrap_or_default();
                let content = service.get(&id).await.map_err(failure)?;
                let count = content.get("files").and_then(Value::as_object).map(|m| m.len()).unwrap_or(0);
                let text = format!(
                    "{} ({} · {count} files)",
                    content.pointer("/info/title").and_then(Value::as_str).unwrap_or_default(),
                    content.pointer("/info/id").and_then(Value::as_str).unwrap_or_default(),
                );
                Ok(ToolResult::text(text).with_output(content))
            }
            Kind::Update => {
                let id = opt_string(&input, "id").unwrap_or_default();
                let info = service
                    .update(
                        &id,
                        UpdateInput {
                            title: opt_string(&input, "title"),
                            files,
                            entry: opt_string(&input, "entry"),
                            session_id: Some(ctx.session_id.clone()),
                        },
                    )
                    .await
                    .map_err(failure)?;
                let content = format!("Updated {} ({}): {}", info.title, info.id, info.url);
                Ok(ToolResult::text(content).with_output(serde_json::to_value(&info).unwrap_or(Value::Null)))
            }
            Kind::Delete => {
                let id = opt_string(&input, "id").unwrap_or_default();
                service.remove(&id).await.map_err(failure)?;
                Ok(ToolResult::text(format!("Deleted {id}")).with_output(json!({ "id": id, "deleted": true })))
            }
        }
    }
}
