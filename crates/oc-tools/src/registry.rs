//! Tool registry, request snapshots and execution (`core/src/tool.ts`, `core/src/tool/runtime.ts`).

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use async_trait::async_trait;
use oc_schema::{PermissionRule, ToolContent};
use serde_json::Value;

use crate::tool::{DefinitionContext, Namespace, Tool, ToolContext, ToolDefinition, ToolError, ToolResult, effective_name, normalized_name};
use crate::util::wildcard;
use crate::validate;

/// `Tool.RegistrationError`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RegistrationError {
    pub name: String,
    pub message: String,
}

impl std::fmt::Display for RegistrationError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for RegistrationError {}

fn segment_ok(segment: &str) -> bool {
    !segment.is_empty() && segment.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
}

fn namespace_error(name: &str) -> Option<RegistrationError> {
    if name.split('.').all(segment_ok) {
        return None;
    }
    Some(RegistrationError { name: name.to_string(), message: format!("Invalid tool namespace: {}", crate::util::js::quote(name)) })
}

/// Validate a registration (`registrationError`).
pub fn registration_error(tool: &dyn Tool) -> Option<RegistrationError> {
    let options = tool.options();
    if let Some(namespace) = &options.namespace {
        if let Some(error) = namespace_error(namespace) {
            return Some(error);
        }
    }
    let name = normalized_name(tool.name());
    if !segment_ok(&name) {
        return Some(RegistrationError { message: format!("Invalid tool name: {name}"), name });
    }
    let id = effective_name(tool.name(), options.namespace.as_deref());
    if !options.codemode && id == "execute" {
        return Some(RegistrationError { name: id, message: "Tool name \"execute\" is reserved for CodeMode".into() });
    }
    let definition = tool.definition(&DefinitionContext::default());
    if !definition.input_schema.is_object() && !definition.input_schema.is_boolean() {
        return Some(RegistrationError {
            message: format!("Invalid tool definition {id}: Expected JsonSchema object"),
            name: id,
        });
    }
    None
}

/// `whollyDisabled(action, rules)`.
pub fn wholly_disabled(action: &str, rules: &[PermissionRule]) -> bool {
    rules
        .iter()
        .rev()
        .find(|rule| wildcard::matches(action, &rule.action))
        .is_some_and(|rule| rule.resource == "*" && rule.effect == oc_schema::Effect::Deny)
}

/// Location-scoped registry. Last valid registration for an effective name wins and keeps
/// the first registration's position (JS `Map.set` semantics).
#[derive(Default, Clone)]
pub struct Registry {
    tools: Vec<(String, Arc<dyn Tool>)>,
    namespaces: Vec<Namespace>,
    errors: Vec<RegistrationError>,
}

impl Registry {
    pub fn new() -> Self {
        Self::default()
    }

    /// `editor.add`. Invalid registrations are logged (`Skipping invalid tool registration`) and skipped.
    pub fn add(&mut self, tool: Arc<dyn Tool>) -> Result<(), RegistrationError> {
        if let Some(error) = registration_error(tool.as_ref()) {
            tracing::error!(name = tool.name(), namespace = ?tool.options().namespace, error = %error.message, "Skipping invalid tool registration");
            self.errors.push(error.clone());
            return Err(error);
        }
        let id = effective_name(tool.name(), tool.options().namespace.as_deref());
        match self.tools.iter_mut().find(|(name, _)| *name == id) {
            Some(slot) => slot.1 = tool,
            None => self.tools.push((id, tool)),
        }
        Ok(())
    }

    /// `editor.namespace`.
    pub fn namespace(&mut self, namespace: Namespace) -> Result<(), RegistrationError> {
        if let Some(error) = namespace_error(&namespace.name) {
            tracing::error!(name = %namespace.name, error = %error.message, "Skipping invalid namespace registration");
            self.errors.push(error.clone());
            return Err(error);
        }
        match self.namespaces.iter_mut().find(|n| n.name == namespace.name) {
            Some(slot) => *slot = namespace,
            None => self.namespaces.push(namespace),
        }
        Ok(())
    }

    pub fn remove(&mut self, id: &str) {
        self.tools.retain(|(name, _)| name != id);
    }

    pub fn get(&self, id: &str) -> Option<Arc<dyn Tool>> {
        self.tools.iter().find(|(name, _)| name == id).map(|(_, t)| t.clone())
    }

    /// Registered tools in registration order, keyed by effective name.
    pub fn list(&self) -> Vec<(String, Arc<dyn Tool>)> {
        self.tools.clone()
    }

    pub fn namespaces(&self) -> &[Namespace] {
        &self.namespaces
    }

    pub fn errors(&self) -> &[RegistrationError] {
        &self.errors
    }

    /// `Tool.snapshot(permissions)`.
    pub fn snapshot(&self, rules: &[PermissionRule]) -> Snapshot {
        let active: Vec<(String, Arc<dyn Tool>)> = self
            .tools
            .iter()
            .filter(|(name, tool)| {
                let action = tool.options().permission.unwrap_or_else(|| name.clone());
                !wholly_disabled(&action, rules)
            })
            .cloned()
            .collect();
        let mut direct: Vec<(String, Arc<dyn Tool>)> = active.iter().filter(|(_, t)| !t.options().codemode).cloned().collect();
        // Plain code-unit comparison (`<`/`>`).
        direct.sort_by(|a, b| {
            let a16: Vec<u16> = a.0.encode_utf16().collect();
            let b16: Vec<u16> = b.0.encode_utf16().collect();
            a16.cmp(&b16)
        });
        let codemode: Vec<(String, Arc<dyn Tool>)> = active.iter().filter(|(_, t)| t.options().codemode).cloned().collect();
        Snapshot {
            direct,
            codemode,
            namespaces: self.namespaces.clone(),
            execute_enabled: !wholly_disabled("execute", rules),
        }
    }
}

/// Plugin `tool.execute.before/after` hooks (owned by core).
#[async_trait]
pub trait ExecuteHooks: Send + Sync {
    /// May rewrite the tool name and input, or reject the call with a `Tool.Error`.
    async fn before(&self, _tool: &mut String, _input: &mut Value, _ctx: &ToolContext) -> Result<(), ToolError> {
        Ok(())
    }
    /// May rewrite the result or error.
    async fn after(&self, _tool: &str, _input: &Value, _result: &mut Result<ToolResult, ToolError>, _ctx: &ToolContext) {}
}

pub struct NoHooks;

impl ExecuteHooks for NoHooks {}

/// Definitions and executors captured for one model request.
#[derive(Clone)]
pub struct Snapshot {
    /// Direct tools sorted by effective name.
    pub direct: Vec<(String, Arc<dyn Tool>)>,
    /// CodeMode tools in registration order (catalog inventory).
    pub codemode: Vec<(String, Arc<dyn Tool>)>,
    pub namespaces: Vec<Namespace>,
    /// `execute` is not wholly disabled (and there is a CodeMode tool to advertise).
    pub execute_enabled: bool,
}

impl Snapshot {
    /// Definitions for the request: direct tools (sorted), after session hooks remove/describe
    /// them, then the CodeMode `execute` definition (supplied by core/codemode) last.
    pub fn definitions(&self, ctx: &DefinitionContext, execute: Option<ToolDefinition>) -> Vec<ToolDefinition> {
        let mut out: Vec<ToolDefinition> = self
            .direct
            .iter()
            .filter(|(_, tool)| tool.advertised(ctx))
            .map(|(name, tool)| {
                let mut definition = tool.definition(ctx);
                definition.name = name.clone();
                definition
            })
            .collect();
        if self.execute_enabled {
            if let Some(execute) = execute {
                out.push(execute);
            }
        }
        out
    }

    /// Registered (pre-hook) definitions.
    pub fn registered_definitions(&self) -> Vec<ToolDefinition> {
        self.definitions(&DefinitionContext::default(), None)
    }

    /// CodeMode tool definitions (for the catalog).
    pub fn codemode_definitions(&self) -> Vec<(ToolDefinition, crate::tool::ToolOptions)> {
        self.codemode
            .iter()
            .map(|(name, tool)| {
                let mut definition = tool.definition(&DefinitionContext::default());
                definition.name = name.clone();
                (definition, tool.options())
            })
            .collect()
    }

    pub fn direct_tool(&self, name: &str) -> Option<Arc<dyn Tool>> {
        self.direct.iter().find(|(n, _)| n == name).map(|(_, t)| t.clone())
    }

    pub fn codemode_tool(&self, name: &str) -> Option<Arc<dyn Tool>> {
        self.codemode.iter().find(|(n, _)| n == name).map(|(_, t)| t.clone())
    }

    /// `Snapshot.execute` for a direct tool call. `advertised` = names that survived the
    /// session hooks for this request. The CodeMode `execute` tool is handled by core.
    pub async fn execute(
        &self,
        name: &str,
        input: Value,
        ctx: ToolContext,
        advertised: Option<&HashSet<String>>,
        hooks: &dyn ExecuteHooks,
    ) -> Result<ToolResult, ToolError> {
        let mut tool_name = name.to_string();
        let mut input = input;
        if let Some(tool) = self.direct_tool(&tool_name) {
            input = tool.prepare_input(input);
        }
        hooks.before(&mut tool_name, &mut input, &ctx).await?;
        if let Some(advertised) = advertised {
            if !advertised.contains(&tool_name) && self.direct_tool(&tool_name).is_some() {
                return Err(ToolError::new(format!("Tool is not available for this request: {tool_name}")));
            }
        }
        let Some(tool) = self.direct_tool(&tool_name) else {
            return Err(ToolError::new(format!(
                "No tool named \"{tool_name}\" is currently available. Please use a tool from the available tool list."
            )));
        };
        execute_tool(tool.as_ref(), &tool_name, input, ctx, hooks).await
    }
}

/// Validate, execute, apply `execute.after`, normalize content and images.
pub async fn execute_tool(tool: &dyn Tool, name: &str, input: Value, ctx: ToolContext, hooks: &dyn ExecuteHooks) -> Result<ToolResult, ToolError> {
    let limits = ctx.config.image;
    let hook_ctx = ctx.clone();
    let mut result = run(tool, name, input.clone(), ctx).await;
    hooks.after(name, &input, &mut result, &hook_ctx).await;
    let mut result = result?;
    result.content = crate::image::normalize_content(normalize_content(result.content, result.output.as_ref()), &limits);
    Ok(result)
}

/// `runtime.execute`: decode input, run, normalize content (no hooks, no image normalization).
pub async fn run(tool: &dyn Tool, name: &str, input: Value, ctx: ToolContext) -> Result<ToolResult, ToolError> {
    let issues = tool.validate(&input);
    if !issues.is_empty() {
        return Err(ToolError::new(validate::format_input_issues(name, &issues, &input)));
    }
    let cancel = ctx.cancel.clone();
    let result = tokio::select! {
        result = tool.execute(input, ctx) => result,
        _ = cancel.cancelled() => Err(ToolError::interrupted()),
    }?;
    Ok(ToolResult { content: normalize_content(result.content, result.output.as_ref()), ..result })
}

/// `normalizeContent`: empty content becomes the stringified output.
pub fn normalize_content(content: Vec<ToolContent>, output: Option<&Value>) -> Vec<ToolContent> {
    if !content.is_empty() {
        return content;
    }
    let text = match output {
        Some(Value::String(s)) => s.clone(),
        Some(value) => serde_json::to_string(value).unwrap_or_default(),
        None => String::new(),
    };
    vec![ToolContent::Text { text }]
}

/// Effective names of the definitions advertised to a request.
pub fn advertised_names(definitions: &[ToolDefinition]) -> HashSet<String> {
    definitions.iter().map(|d| d.name.clone()).collect()
}

/// Convenience map of effective name → tool.
pub fn by_name(tools: &[(String, Arc<dyn Tool>)]) -> HashMap<String, Arc<dyn Tool>> {
    tools.iter().cloned().collect()
}
