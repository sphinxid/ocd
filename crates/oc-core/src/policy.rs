//! The configuration policy plugin (`core/src/config/plugin/policy.ts`, `opencode.config.policy`):
//! top-level `policies` (with the deprecated `experimental.policies` and V1 provider lists merged
//! in by normalization) remove denied providers, web search providers, MCP servers and skills, and deny
//! matching tool use. A Console organization's statements arrive with its integration document.
//!
//! `integration.use` names `<integration>#<account>`, the account behind the integration's active stored
//! credential ([`accounts`]); a provider belongs to the well-known origin that defines it, else to its
//! integration, else to itself.

use std::collections::{BTreeMap, HashMap};

use oc_ai::catalog::types::Activation;
use oc_ai::catalog::{CatalogPlugin, ProviderEditor};
use oc_config::policy::{self, Statement};
use oc_config::schema::{Entry, PolicyAction, PolicyEffect};

/// The account behind each integration's active stored credential (`ManagedPolicy.accounts`).
pub type Accounts = BTreeMap<String, Option<String>>;

/// `ManagedPolicy.accounts(credentials.all())`: credentials list each integration's active credential last.
pub fn accounts(credentials: &[oc_ai::credentials::Credential]) -> Accounts {
    credentials.iter().map(|c| (c.integration_id.clone(), policy::account(c.value.metadata()))).collect()
}

/// `integrationDenied`: `integration.use` denies what `integration_id` supplies through its active account.
pub fn integration_denied(statements: &[Statement], accounts: &Accounts, integration_id: &str) -> bool {
    policy::integration_denied(statements, integration_id, accounts.get(integration_id).and_then(|a| a.as_deref()))
}

/// Built-in plugin id.
pub const PLUGIN_ID: &str = "opencode.config.policy";

/// `ManagedPolicy.statements(entries)`: Console organization policies arrive as an integration document.
pub fn statements(entries: &[Entry]) -> Vec<Statement> {
    policy::statements(entries)
}

/// The provider transform: a provider is removed when `provider.use` denies it, or `integration.use` denies the
/// integration supplying it (the well-known origin that defines it, else its integration, else its own id).
pub struct Providers {
    pub statements: Vec<Statement>,
    pub accounts: Accounts,
    /// `ConfigIntegration.suppliers(entries)`.
    pub suppliers: HashMap<String, String>,
}

impl CatalogPlugin for Providers {
    fn providers(&mut self, editor: &mut ProviderEditor) {
        if self.statements.is_empty() {
            return;
        }
        let mut denied: Vec<String> = Vec::new();
        for id in editor.ids() {
            let supplier = self
                .suppliers
                .get(&id)
                .cloned()
                .or_else(|| editor.get(&id).and_then(|r| r.provider.integration_id.clone()))
                .unwrap_or_else(|| id.clone());
            let deny = policy::decision(&self.statements, PolicyAction::ProviderUse, &id) == PolicyEffect::Deny
                || integration_denied(&self.statements, &self.accounts, &supplier);
            if !deny {
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

/// The MCP transform: servers denied `mcp.use` on their name are removed.
pub fn mcp_allowed(statements: &[Statement], name: &str) -> bool {
    policy::mcp_allowed(statements, name)
}

/// The skill transform: skills denied `skill.use` on their id are removed.
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
            integration: None,
            path: None,
            info: Info::from_value(&json!({ "policies": policies })).unwrap(),
            wellknown: None,
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
        let global = doc(json!([{ "action": "mcp.use", "resource": "*", "effect": "deny" }]));
        // A legacy prefixed `integration.use` statement converts to `mcp.use` when decoded.
        let project = doc(json!([{ "action": "integration.use", "resource": "mcp:github", "effect": "allow" }]));
        let statements = statements(&[global, project]);
        assert!(!mcp_allowed(&statements, "github"));
        assert!(skill_allowed(&statements, "github"));
        let statements = super::statements(&[doc(json!([{ "action": "skill.use", "resource": "review", "effect": "deny" }]))]);
        assert!(!skill_allowed(&statements, "review"));
        assert!(mcp_allowed(&statements, "review"));
    }

    #[test]
    fn integration_use_names_the_active_account() {
        let statements =
            statements(&[doc(json!([{ "action": "integration.use", "resource": "opencode#org_1", "effect": "deny" }]))]);
        let mut accounts = Accounts::new();
        assert!(!integration_denied(&statements, &accounts, "opencode"));
        accounts.insert("opencode".into(), Some("org_1".into()));
        assert!(integration_denied(&statements, &accounts, "opencode"));
        accounts.insert("opencode".into(), Some("org_2".into()));
        assert!(!integration_denied(&statements, &accounts, "opencode"));
    }
}
