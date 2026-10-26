//! The configuration policy plugin (`core/src/config/plugin/policy.ts`, `opencode.config.policy`):
//! top-level `policies` (with the deprecated `experimental.policies` and V1 provider lists merged
//! in by normalization) remove denied providers, MCP servers and skills, and deny matching tool use.
//! Connected-Console organization statements are not ported, so only authored documents apply.

use oc_ai::catalog::types::Activation;
use oc_ai::catalog::{CatalogPlugin, ProviderEditor};
use oc_config::policy::{self, ManagedState, Statement};
use oc_config::schema::{Entry, PolicyAction, PolicyEffect};

/// Built-in plugin id.
pub const PLUGIN_ID: &str = "opencode.config.policy";

/// `ManagedPolicy.statements(entries, managed.current())` without Console statements.
pub fn statements(entries: &[Entry]) -> Vec<Statement> {
    policy::statements(entries, &ManagedState::default())
}

/// The provider transform: providers whose last matching `provider.use` statement denies are removed.
pub struct Providers(pub Vec<Statement>);

impl CatalogPlugin for Providers {
    fn providers(&mut self, editor: &mut ProviderEditor) {
        if self.0.is_empty() {
            return;
        }
        let mut denied: Vec<String> = Vec::new();
        for id in editor.ids() {
            if policy::find(&self.0, PolicyAction::ProviderUse, &id).map(|p| p.effect)
                != Some(PolicyEffect::Deny)
            {
                continue;
            }
            // Skip unconfigured catalog entries; deny-all policies would otherwise log the whole catalog.
            let enabled = editor
                .get(&id)
                .is_some_and(|r| r.provider.activation == Activation::Enabled);
            editor.remove(&id);
            if enabled {
                denied.push(id);
            }
        }
        if !denied.is_empty() {
            tracing::debug!(providers = ?denied, "providers denied by policy");
        }
    }
}

/// The MCP transform: servers denied `integration.use` on `mcp:<name>` are removed.
pub fn mcp_allowed(statements: &[Statement], name: &str) -> bool {
    policy::mcp_allowed(statements, name)
}

/// The skill transform: skills denied `integration.use` on `skill:<id>` are removed.
pub fn skill_allowed(statements: &[Statement], id: &str) -> bool {
    policy::skill_allowed(statements, id)
}

/// The `permission.evaluate` hook: the first resource whose last matching `tool.use` statement
/// denies turns the evaluation into `deny` with the statement's message.
pub fn evaluate(statements: &[Statement], event: &mut serde_json::Value) {
    let Some(action) = event.get("action").and_then(serde_json::Value::as_str) else {
        return;
    };
    let resources: Vec<String> = event
        .get("resources")
        .and_then(serde_json::Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(|r| r.as_str().map(String::from))
                .collect()
        })
        .unwrap_or_default();
    let Some(denied) = policy::tool_denial(statements, action, &resources) else {
        return;
    };
    event["effect"] = serde_json::Value::String("deny".into());
    event["message"] = serde_json::Value::String(denied.message.clone());
}

#[cfg(test)]
mod tests {
    use super::*;
    use oc_config::schema::Info;
    use serde_json::json;

    fn doc(policies: serde_json::Value) -> Entry {
        Entry::Document {
            path: None,
            info: Info::from_value(&json!({ "policies": policies })).unwrap(),
        }
    }

    #[test]
    fn denies_tool_use_with_the_statement_message() {
        let entries = vec![doc(
            json!([{ "action": "tool.use", "resource": "shell:rm *", "effect": "deny" }]),
        )];
        let statements = statements(&entries);
        let mut event =
            json!({ "action": "shell", "resources": ["ls", "rm -rf x"], "effect": "ask" });
        evaluate(&statements, &mut event);
        assert_eq!(event["effect"], "deny");
        assert_eq!(event["message"], "Blocked by configuration policy");
        let mut event = json!({ "action": "shell", "resources": ["ls"], "effect": "ask" });
        evaluate(&statements, &mut event);
        assert_eq!(event["effect"], "ask");
        assert!(event.get("message").is_none());
    }

    #[test]
    fn global_policy_outranks_project_policy() {
        // Entries are lowest priority first; authored documents reverse so the global one has the final say.
        let global =
            doc(json!([{ "action": "integration.use", "resource": "mcp:*", "effect": "deny" }]));
        let project = doc(
            json!([{ "action": "integration.use", "resource": "mcp:github", "effect": "allow" }]),
        );
        let statements = statements(&[global, project]);
        assert!(!mcp_allowed(&statements, "github"));
        assert!(skill_allowed(&statements, "github"));
    }
}
