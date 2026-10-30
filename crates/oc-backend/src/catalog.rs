//! Location registries: skills (delegated to `oc_core::skills`) and commands (`CommandPlugin`,
//! `ConfigCommandPlugin`, MCP prompts).

use indexmap::IndexMap;
use oc_config::schema::Command;
use oc_config::sources::{self, Skill};
use serde_json::{Value, json};

use crate::locations::Loc;

pub const PROMPT_INITIALIZE: &str = include_str!("../../../assets/prompt/command/initialize.txt");
pub const PROMPT_REVIEW: &str = include_str!("../../../assets/prompt/command/review.txt");

/// Every skill registered at a location (built-ins, plugin transforms, then compatibility roots
/// and configured directories and remote sources).
pub async fn skills(loc: &Loc, version: &str, global: &oc_util::Global, plugins: &oc_core::plugins::Plugins) -> Vec<Skill> {
    plugins.skills(&loc.location, global, version).await
}

/// `Skill.Info` JSON.
pub fn skill_json(skill: &Skill) -> Value {
    let mut v = json!({"id": skill.id, "name": skill.name});
    if let Some(d) = &skill.description {
        v["description"] = json!(d);
    }
    if let Some(a) = skill.autoinvoke {
        v["autoinvoke"] = json!(a);
    }
    v["path"] = json!(skill.path.display().to_string());
    v["content"] = json!(skill.content);
    v
}

/// A registered command.
pub enum CommandDef {
    Init,
    Review,
    Mcp(oc_mcp::Prompt),
    /// `/goal` (the built-in `opencode.goal` plugin).
    Goal,
    Config(Command),
    /// A plugin command: its `execute` function handle in the plugin host.
    Plugin(String),
}

/// `mcpCommandName`: `<server>:<prompt>` with `[^a-zA-Z0-9_-]` → `_`.
pub fn mcp_command_name(server: &str, prompt: &str) -> String {
    let sanitize = |v: &str| v.chars().map(|c| if c.is_ascii_alphanumeric() || c == '_' || c == '-' { c } else { '_' }).collect::<String>();
    format!("{}:{}", sanitize(server), sanitize(prompt))
}

/// `promptMessageText`: string content or `{type:"text", text}`.
pub fn prompt_message_text(content: &Value) -> String {
    match content {
        Value::String(s) => s.clone(),
        Value::Object(o) if o.get("type").and_then(Value::as_str) == Some("text") => {
            o.get("text").and_then(Value::as_str).unwrap_or_default().to_string()
        }
        _ => String::new(),
    }
}

/// Commands registered at a location: built-ins, MCP prompts, then configuration (later names
/// replace earlier). Without plugin transforms; see `Engine::commands`.
pub fn commands(loc: &Loc, prompts: &[oc_mcp::Prompt]) -> IndexMap<String, (Option<String>, CommandDef)> {
    let mut out = native(prompts);
    configured(loc, &mut out);
    out
}

/// The native commands registered before plugins (`opencode.command`, MCP prompts, `opencode.goal`).
pub fn native(prompts: &[oc_mcp::Prompt]) -> IndexMap<String, (Option<String>, CommandDef)> {
    let mut out = IndexMap::new();
    out.insert("init".to_string(), (Some("guided AGENTS.md setup".to_string()), CommandDef::Init));
    out.insert(
        "review".to_string(),
        (Some("review changes [commit|branch|pr], defaults to uncommitted".to_string()), CommandDef::Review),
    );
    for prompt in prompts {
        out.insert(mcp_command_name(&prompt.server, &prompt.name), (prompt.description.clone(), CommandDef::Mcp(prompt.clone())));
    }
    // `opencode.goal` registers its command after the command plugin's.
    out.insert(oc_core::goal::COMMAND.to_string(), (Some(oc_core::goal::DESCRIPTION.to_string()), CommandDef::Goal));
    out
}

/// Configured commands (`opencode.config.command`, a `post` plugin).
pub fn configured(loc: &Loc, out: &mut IndexMap<String, (Option<String>, CommandDef)>) {
    for (name, command) in sources::commands(&loc.location.entries) {
        out.insert(name, (command.description.clone(), CommandDef::Config(command)));
    }
}

/// `append(template, input)` of the built-in command plugin.
pub fn append(template: &str, input: &str) -> String {
    let value = input.trim();
    if template.contains("$ARGUMENTS") {
        return template.replace("$ARGUMENTS", value);
    }
    [template, value].into_iter().filter(|s| !s.is_empty()).collect::<Vec<_>>().join("\n\n")
}

/// Shell used for command template interpolation (`ShellSelect.resolve({priority:"config"})`).
pub fn template_shell(loc: &Loc) -> String {
    oc_config::fold::shell(&loc.location.entries)
        .map(str::to_string)
        .or_else(|| std::env::var("SHELL").ok().filter(|s| !s.is_empty()))
        .unwrap_or_else(|| "/bin/sh".into())
}
