//! `subagent` (`core/src/tool/plugin/subagent.ts`). Session/job work is delegated to `ToolHost`.

use async_trait::async_trait;
use oc_schema::ModelRef;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::host::{JobBlockResult, JobRecovery, JobStatus};
use crate::tool::{DefinitionContext, ErrorCause, Tool, ToolContext, ToolDefinition, ToolError, ToolOptions, ToolResult, decode, meta};
use crate::util::js;

pub const NO_TEXT: &str = "Subagent completed without a text response.";

pub const DESCRIPTION: &str = "Spawns an agent in a child session to work on the specified task.\nThe output includes a sessionID you can pass back later to continue that specific conversation with the subagent.\nNew child sessions start with fresh context, so include all relevant context and instructions when you don't pass a sessionID.\nForeground (default) runs the subagent to completion and returns its final response.\nBackground mode (background=true) launches it asynchronously and returns immediately; you are notified when it finishes.\nUse background only for independent work that can run while you continue elsewhere.";

/// Description after the session hook (`Available subagents:` list, sorted by id).
pub fn description(subagents: Option<&[crate::tool::SubagentSummary]>) -> String {
    let Some(list) = subagents.filter(|l| !l.is_empty()) else { return DESCRIPTION.into() };
    let mut sorted = list.to_vec();
    sorted.sort_by(|a, b| js::locale_compare(&a.id, &b.id));
    let mut lines = vec![DESCRIPTION.to_string(), String::new(), "Available subagents:".into()];
    for agent in sorted {
        lines.push(format!(
            "- {}: {}",
            agent.id,
            agent.description.unwrap_or_else(|| "This subagent should only be called when explicitly requested.".into())
        ));
    }
    lines.join("\n")
}

pub fn background_text(session_id: &str) -> String {
    [
        format!("The subagent is working in the background (sessionID: {session_id}). You will be notified automatically when it finishes."),
        "DO NOT sleep, poll for progress, ask the subagent for status, or duplicate this subagent's work; avoid working with the same files or topics it is using.".into(),
        "Work on non-overlapping tasks, or briefly tell the user what you launched and end your response.".into(),
        "To send it follow-ups or change its instructions while it runs, call subagent again with this sessionID.".into(),
    ]
    .join("\n")
}

/// `Model.Ref.parse`.
pub fn parse_model(input: &str) -> Option<ModelRef> {
    let provider_end = input.find('/').filter(|i| *i > 0)?;
    let provider = &input[..provider_end];
    let rest = &input[provider_end + 1..];
    let (id, variant) = match rest.find('#') {
        Some(i) => (&rest[..i], Some(&rest[i + 1..])),
        None => (rest, None),
    };
    if id.is_empty() || provider.contains('#') || variant.is_some_and(|v| v.is_empty() || v.contains('#')) {
        return None;
    }
    Some(ModelRef { id: id.into(), provider_id: provider.into(), variant: variant.map(String::from) })
}

#[derive(Deserialize)]
struct Input {
    agent: String,
    description: String,
    prompt: String,
    model: Option<String>,
    #[serde(rename = "sessionID")]
    session_id: Option<String>,
    background: Option<bool>,
}

pub struct SubagentTool;

impl SubagentTool {
    pub(crate) async fn resolve_model(&self, ctx: &ToolContext, input: &str) -> Result<ModelRef, ToolError> {
        let reference = parse_model(input).ok_or_else(|| {
            ToolError::new(format!("Invalid model \"{input}\". Use \"providerID/modelID\" or \"providerID/modelID#variant\"."))
        })?;
        let unable = |m: String| ToolError::with_cause("Unable to list models", ErrorCause::Error(m));
        let matches = |m: &crate::host::ModelSummary| m.provider_id == reference.provider_id && m.id == reference.id;
        let models = ctx.host.models_available().await.map_err(unable)?;
        let Some(model) = models.iter().find(|m| matches(m)) else {
            // `models.available(["image", "decision"])`: image models are listed first.
            let mut other = None;
            for (kind, article) in [("image", "an image"), ("decision", "a decision")] {
                if ctx.host.models_list(&[kind.to_string()]).await.map_err(unable)?.iter().any(matches) {
                    other = Some(article);
                    break;
                }
            }
            return Err(ToolError::new(match other {
                Some(article) => format!(
                    "Model \"{}/{}\" is {article} model; subagents need a chat model. Use the models tool to find one.",
                    reference.provider_id, reference.id
                ),
                None => format!(
                    "Model \"{}/{}\" is not available. Use the models tool to see what is available.",
                    reference.provider_id, reference.id
                ),
            }));
        };
        if let Some(variant) = &reference.variant {
            if !model.variants.iter().any(|v| v == variant) {
                return Err(ToolError::new(if model.variants.is_empty() {
                    format!("Model \"{}/{}\" has no variants. Omit the variant.", reference.provider_id, reference.id)
                } else {
                    format!(
                        "Variant \"{variant}\" is not available for \"{}/{}\". Available: {}.",
                        reference.provider_id,
                        reference.id,
                        model.variants.join(", ")
                    )
                }));
            }
        }
        Ok(reference)
    }
}

#[async_trait]
impl Tool for SubagentTool {
    fn name(&self) -> &str {
        "subagent"
    }

    fn options(&self) -> ToolOptions {
        ToolOptions::direct()
    }

    fn definition(&self, ctx: &DefinitionContext) -> ToolDefinition {
        super::definition("subagent", description(ctx.subagents.as_deref()), include_str!("../../schemas/subagent.json"))
    }

    /// `tool.execute.before`: strip empty `model` / `sessionID`.
    fn prepare_input(&self, input: Value) -> Value {
        let Value::Object(mut map) = input else { return input };
        if map.get("model") == Some(&json!("")) {
            map.remove("model");
        }
        if map.get("sessionID") == Some(&json!("")) {
            map.remove("sessionID");
        }
        Value::Object(map)
    }

    async fn execute(&self, input: Value, ctx: ToolContext) -> Result<ToolResult, ToolError> {
        let input: Input = decode(input)?;
        let host = &ctx.host;
        let parent = host
            .session_get(&ctx.session_id)
            .await
            .map_err(|m| ToolError::with_cause(format!("Parent session not found: {}", ctx.session_id), ErrorCause::Error(m)))?;
        let mut current = parent.clone();
        let mut depth = 0u64;
        while let Some(parent_id) = current.parent_id.clone() {
            depth += 1;
            current = host
                .session_get(&parent_id)
                .await
                .map_err(|m| ToolError::with_cause(format!("Parent session not found: {parent_id}"), ErrorCause::Error(m)))?;
        }
        let limit = ctx.config.subagent_depth.unwrap_or(1);
        if depth >= limit {
            return Err(ToolError::new(format!(
                "Subagent depth limit reached ({limit}). Increase \"experimental.subagent_depth\" to allow nested subagents."
            )));
        }
        let agent = host
            .agent_resolve(&input.agent)
            .await
            .map_err(|m| ToolError::with_cause(format!("Unknown agent: {}", input.agent), ErrorCause::Error(m)))?
            .ok_or_else(|| ToolError::new(format!("Unknown agent: {}", input.agent)))?;
        if agent.mode == "primary" {
            return Err(ToolError::new(format!("Agent {} cannot run as a subagent", input.agent)));
        }
        super::assert(&ctx, "subagent", vec![agent.id.clone()], Some(vec![agent.id.clone()]), None)
            .await
            .map_err(super::perm(format!("Subagent denied: {}", agent.id)))?;
        let existing = match &input.session_id {
            None => None,
            Some(id) => Some(
                host.session_get(id)
                    .await
                    .map_err(|m| ToolError::with_cause(format!("Subagent session not found: {id}"), ErrorCause::Error(m)))?,
            ),
        };
        if let Some(existing) = &existing {
            if existing.parent_id.as_deref() != Some(ctx.session_id.as_str()) {
                return Err(ToolError::new(format!("Session {} is not a child of the current session", existing.id)));
            }
        }
        let override_model = match &input.model {
            Some(m) => Some(self.resolve_model(&ctx, m).await?),
            None => None,
        };
        if let Some(existing) = &existing {
            let switched = existing.agent.as_deref() != Some(agent.id.as_str());
            let model = override_model.clone().or_else(|| if switched { agent.model.clone() } else { None });
            let fail = |m: String| ToolError::with_cause(format!("Failed to switch subagent session: {}", existing.id), ErrorCause::Error(m));
            if switched {
                host.session_switch_agent(&existing.id, &agent.id).await.map_err(fail)?;
            }
            if let Some(model) = model {
                host.session_switch_model(&existing.id, model).await.map_err(fail)?;
            }
        }
        let child = match &existing {
            Some(existing) => existing.clone(),
            None => {
                let model = override_model.clone().or_else(|| agent.model.clone()).or_else(|| parent.model.clone());
                host.session_create(&ctx.session_id, &input.description, &input.agent, model)
                    .await
                    .map_err(|m| ToolError::with_cause(format!("Parent session not found: {}", ctx.session_id), ErrorCause::Error(m)))?
            }
        };
        let background = input.background == Some(true);
        ctx.progress(meta(vec![("sessionID", json!(child.id)), ("status", json!("running"))]));
        let text = if existing.is_none() { format!("You are a subagent spawned by another session.\n{}", input.prompt) } else { input.prompt.clone() };
        host.session_prompt(&child.id, &text, (background && existing.is_none()).then_some(false))
            .await
            .map_err(|m| ToolError::with_cause(format!("Failed to prompt subagent: {}", child.id), ErrorCause::Error(m)))?;
        let recovery = JobRecovery::Subagent {
            parent_session_id: ctx.session_id.clone(),
            child_session_id: child.id.clone(),
            agent: agent.name.clone(),
            description: input.description.clone(),
        };
        host.subagent_job_start(recovery.clone()).await.map_err(|m| ToolError::with_cause(format!("Failed to prompt subagent: {}", child.id), ErrorCause::Error(m)))?;
        let finish = |status: &str, output: String| {
            let content = if status == "completed" {
                format!("<subagent sessionID=\"{}\" state=\"completed\">\n{output}\n</subagent>", child.id)
            } else {
                output.clone()
            };
            ToolResult::text(content)
                .with_output(json!({"sessionID": child.id, "status": status, "output": output}))
                .with_metadata(meta(vec![("sessionID", json!(child.id)), ("status", json!(status))]))
        };
        if background {
            let _ = host.subagent_job_background(recovery).await;
            return Ok(finish("running", background_text(&child.id)));
        }
        let result = tokio::select! {
            result = host.job_block(&child.id, &ctx.session_id) => result,
            _ = ctx.cancel.cancelled() => {
                let _ = host.session_interrupt(&child.id).await;
                let _ = host.job_cancel(&child.id).await;
                return Err(ToolError::interrupted());
            }
        };
        let info = match result {
            Ok(Some(JobBlockResult::Backgrounded(info))) => {
                let _ = host.subagent_job_notify(recovery, info.started_at).await;
                return Ok(finish("running", background_text(&child.id)));
            }
            Ok(Some(JobBlockResult::Finished(info))) => Some(info),
            Ok(None) => None,
            Err(m) => return Err(ToolError::new(format!("Subagent failed (sessionID: {}): {m}", child.id))),
        };
        if let Some(info) = &info {
            if info.status == JobStatus::Error {
                return Err(ToolError::new(format!(
                    "Subagent failed (sessionID: {}): {}",
                    child.id,
                    info.error.clone().unwrap_or_else(|| "unknown error".into())
                )));
            }
            if info.status == JobStatus::Cancelled {
                return Err(ToolError::new(format!("Subagent cancelled (sessionID: {})", child.id)));
            }
        }
        Ok(finish("completed", info.and_then(|i| i.output).unwrap_or_else(|| NO_TEXT.into())))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tool::SubagentSummary;

    #[test]
    fn model_refs() {
        assert_eq!(parse_model("openai/gpt-5#high").unwrap(), ModelRef { id: "gpt-5".into(), provider_id: "openai".into(), variant: Some("high".into()) });
        assert_eq!(parse_model("a/b/c").unwrap().id, "b/c");
        assert!(parse_model("/x").is_none());
        assert!(parse_model("nope").is_none());
        assert!(parse_model("a/b#").is_none());
        assert!(parse_model("a/#v").is_none());
    }

    #[test]
    fn hook_description() {
        let list = vec![
            SubagentSummary { id: "review".into(), description: Some("Reviews code".into()) },
            SubagentSummary { id: "explore".into(), description: None },
        ];
        assert_eq!(
            description(Some(&list)),
            format!("{DESCRIPTION}\n\nAvailable subagents:\n- explore: This subagent should only be called when explicitly requested.\n- review: Reviews code")
        );
        assert_eq!(description(Some(&[])), DESCRIPTION);
    }

    #[test]
    fn strips_empty_strings() {
        let out = SubagentTool.prepare_input(json!({"agent": "a", "model": "", "sessionID": ""}));
        assert_eq!(out, json!({"agent": "a"}));
    }
}
