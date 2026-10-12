//! `write` (`core/src/tool/plugin/write.ts`).

use std::sync::Arc;

use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::file_access::{self, Kind};
use crate::file_diff::file_diff;
use crate::file_mutation::{self, FileError};
use crate::formatter::Formatter;
use crate::tool::{DefinitionContext, ErrorCause, Tool, ToolContext, ToolDefinition, ToolError, ToolOptions, ToolResult, decode, meta};
use crate::util::bom;

pub const DESCRIPTION: &str = "Writes a file to the local filesystem, overwriting if one exists.\n\nMissing parent directories are created automatically.\n\nUse this tool to create new files or overwrite existing files. For partial changes, use the edit tool instead.";

#[derive(Deserialize)]
struct Input {
    path: String,
    content: String,
}

pub struct WriteTool {
    formatter: Arc<Formatter>,
}

impl WriteTool {
    pub fn new(formatter: Arc<Formatter>) -> Self {
        Self { formatter }
    }
}

fn cause(error: FileError) -> ErrorCause {
    ErrorCause::Error(match error {
        FileError::NotFound => String::new(),
        FileError::WrongKind(_) => String::new(),
        FileError::Failed(m) => m,
    })
}

#[async_trait]
impl Tool for WriteTool {
    fn name(&self) -> &str {
        "write"
    }

    fn options(&self) -> ToolOptions {
        ToolOptions { permission: Some("edit".into()), ..ToolOptions::direct() }
    }

    fn definition(&self, _ctx: &DefinitionContext) -> ToolDefinition {
        super::definition("write", DESCRIPTION, include_str!("../../schemas/write.json"))
    }

    fn advertised(&self, ctx: &DefinitionContext) -> bool {
        !super::patch::use_patch(ctx)
    }

    async fn execute(&self, input: Value, ctx: ToolContext) -> Result<ToolResult, ToolError> {
        let input: Input = decode(input)?;
        let leaf = format!("Unable to write {}", input.path);
        let target = file_access::resolve(&ctx, &input.path, Some(Kind::File));
        file_access::authorize_external(&ctx, &[&target], None).await.map_err(super::perm(leaf.clone()))?;
        let current = match file_mutation::read_text(&target.absolute) {
            Ok(text) => Some(text),
            Err(FileError::NotFound) => None,
            Err(error) => return Err(ToolError::with_cause(leaf, cause(error))),
        };
        let next = bom::split(&input.content);
        let preview = file_diff(
            &target.resource,
            current.as_ref().map(|c| c.text.as_str()).unwrap_or(""),
            &next.text,
            if current.is_some() { "modified" } else { "added" },
        );
        super::assert(&ctx, "edit", vec![target.resource.clone()], Some(vec!["*".into()]), Some(meta(vec![("files", json!([preview.to_json()]))])))
            .await
            .map_err(super::perm(leaf.clone()))?;
        let result = file_mutation::write_text_preserving_bom(&target.absolute, &target.resource, &input.content)
            .await
            .map_err(|e| ToolError::with_cause(leaf.clone(), cause(e)))?;
        let has_bom = file_mutation::read_text(&target.absolute).map_err(|e| ToolError::with_cause(leaf.clone(), cause(e)))?.bom;
        if super::format_file(&self.formatter, &ctx, &target.absolute).await {
            file_mutation::sync_text_bom(&target.absolute, has_bom).map_err(|e| ToolError::with_cause(leaf.clone(), cause(e)))?;
        }
        ctx.host.files_changed(&ctx.session_id, std::slice::from_ref(&target.absolute)).await;
        let text = format!("{} file successfully: {}", if result.existed { "Wrote" } else { "Created" }, result.resource);
        Ok(ToolResult::text(text).with_output(result.to_json()))
    }
}
