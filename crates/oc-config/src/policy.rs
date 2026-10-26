//! Managed policy evaluation (`core/src/managed-policy.ts`, `config/plugin/policy.ts`), wildcard
//! matching (`core/src/util/wildcard.ts`) and the CLI updater `update` policy reader
//! (`cli/src/services/updater.ts`).

use std::path::Path;

use serde::Serialize;

use crate::schema::{Entry, Policy, PolicyAction, PolicyEffect, UpdatePolicy};

/// `Wildcard.match(input, pattern)`: `*` → `.*`, `?` → `.`, a trailing `" *"` also matches
/// nothing; backslashes normalized to `/`; case-insensitive on Windows; dot matches newlines.
pub fn wildcard_match(input: &str, pattern: &str) -> bool {
    let normalized = input.replace('\\', "/");
    let mut escaped = String::new();
    for c in pattern.replace('\\', "/").chars() {
        match c {
            '.' | '+' | '^' | '$' | '{' | '}' | '(' | ')' | '|' | '[' | ']' => {
                escaped.push('\\');
                escaped.push(c);
            }
            '*' => escaped.push_str(".*"),
            '?' => escaped.push('.'),
            // characters special to the Rust regex syntax but not to JavaScript's
            '#' | '&' | '-' | '~' => {
                escaped.push('\\');
                escaped.push(c);
            }
            c => escaped.push(c),
        }
    }
    if let Some(stripped) = escaped.strip_suffix(" .*") {
        escaped = format!("{stripped}( .*)?");
    }
    let flags = if cfg!(windows) { "(?si)" } else { "(?s)" };
    regex::Regex::new(&format!("{flags}^{escaped}$")).is_ok_and(|re| re.is_match(&normalized))
}

/// Connected-Console policy state.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ManagedState {
    pub statements: Vec<Policy>,
    /// Organization name for denial messages, when the connection knows it.
    pub organization: Option<String>,
}

/// A policy statement tagged with its denial message.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Statement {
    pub action: PolicyAction,
    pub resource: String,
    pub effect: PolicyEffect,
    pub message: String,
}

pub const CONFIGURATION_POLICY_MESSAGE: &str = "Blocked by configuration policy";

/// `ManagedPolicy.statements`: authored documents reversed (global outranks project), then the
/// organization's statements.
pub fn statements(entries: &[Entry], organization: &ManagedState) -> Vec<Statement> {
    let mut out: Vec<Statement> = entries
        .iter()
        .rev()
        .filter_map(Entry::info)
        .flat_map(|i| i.policies.iter().flatten())
        .map(|p| Statement {
            action: p.action,
            resource: p.resource.clone(),
            effect: p.effect,
            message: CONFIGURATION_POLICY_MESSAGE.into(),
        })
        .collect();
    let message = match &organization.organization {
        Some(org) if !org.is_empty() => format!("Blocked by {org}'s policy"),
        _ => "Blocked by your organization's policy".to_string(),
    };
    out.extend(organization.statements.iter().map(|p| Statement {
        action: p.action,
        resource: p.resource.clone(),
        effect: p.effect,
        message: message.clone(),
    }));
    out
}

/// Last matching statement for `action` and `resource`.
pub fn find<'a>(policies: &'a [Statement], action: PolicyAction, resource: &str) -> Option<&'a Statement> {
    policies.iter().rev().find(|p| p.action == action && wildcard_match(resource, &p.resource))
}

/// `ManagedPolicy.decision`: last match wins, default allow.
pub fn decision(policies: &[Statement], action: PolicyAction, resource: &str) -> PolicyEffect {
    find(policies, action, resource).map(|p| p.effect).unwrap_or(PolicyEffect::Allow)
}

/// `provider.use` against a provider id (deny removes the provider).
pub fn provider_allowed(policies: &[Statement], provider_id: &str) -> bool {
    find(policies, PolicyAction::ProviderUse, provider_id).map(|p| p.effect) != Some(PolicyEffect::Deny)
}

/// `integration.use` against `mcp:<name>`.
pub fn mcp_allowed(policies: &[Statement], name: &str) -> bool {
    decision(policies, PolicyAction::IntegrationUse, &format!("mcp:{name}")) != PolicyEffect::Deny
}

/// `integration.use` against `skill:<id>`.
pub fn skill_allowed(policies: &[Statement], id: &str) -> bool {
    decision(policies, PolicyAction::IntegrationUse, &format!("skill:{id}")) != PolicyEffect::Deny
}

/// Permission evaluate hook: the first resource whose last matching `tool.use` statement denies
/// `` `${action}:${resource}` ``; the caller turns the effect into `deny` with this message.
/// Policies never grant.
pub fn tool_denial<'a>(policies: &'a [Statement], action: &str, resources: &[String]) -> Option<&'a Statement> {
    resources
        .iter()
        .map(|r| find(policies, PolicyAction::ToolUse, &format!("{action}:{r}")))
        .find(|p| p.is_some_and(|p| p.effect == PolicyEffect::Deny))
        .flatten()
}

/// `decodePolicy(text)` from the CLI updater.
pub fn decode_update_policy(text: &str) -> Option<UpdatePolicy> {
    let input = oc_util::jsonc::parse(text).ok()?;
    let o = input.as_object()?;
    if let Some(value) = o.get("update") {
        return value.as_str().and_then(UpdatePolicy::parse);
    }
    match o.get("autoupdate")? {
        serde_json::Value::Bool(false) => Some(UpdatePolicy::Disable),
        serde_json::Value::String(s) if s == "notify" => Some(UpdatePolicy::Notify),
        serde_json::Value::Bool(true) => Some(UpdatePolicy::Auto),
        _ => None,
    }
}

/// Updater policy: `G/config.json`, `G/opencode.json`, `G/opencode.jsonc`; last decodable wins;
/// default `notify`. Project-level values are ignored.
pub fn read_update_policy(global_config: &Path) -> UpdatePolicy {
    ["config.json", "opencode.json", "opencode.jsonc"]
        .iter()
        .filter_map(|n| std::fs::read_to_string(global_config.join(n)).ok())
        .filter_map(|t| decode_update_policy(&t))
        .last()
        .unwrap_or(UpdatePolicy::Notify)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::schema::Info;

    #[test]
    fn wildcard() {
        assert!(wildcard_match("shell:git push origin", "shell:git push *"));
        assert!(wildcard_match("shell:git push", "shell:git push *"));
        assert!(!wildcard_match("shell:git pushx", "shell:git push *"));
        assert!(wildcard_match("a.b", "a.b"));
        assert!(!wildcard_match("axb", "a.b"));
        assert!(wildcard_match("abc", "a?c"));
        assert!(wildcard_match("C:\\x\\y", "C:/x/*"));
        assert!(wildcard_match("line1\nline2", "line1*"));
        assert!(wildcard_match("a-b#c", "a-b#c"));
        assert!(wildcard_match("anthropic", "*"));
    }

    fn doc(policies: Vec<Policy>) -> Entry {
        Entry::Document {
            path: None,
            info: Info { policies: Some(policies), ..Default::default() },
        }
    }

    fn pol(action: PolicyAction, resource: &str, effect: PolicyEffect) -> Policy {
        Policy { action, resource: resource.into(), effect }
    }

    #[test]
    fn statement_ordering_and_messages() {
        let project = doc(vec![pol(PolicyAction::ProviderUse, "openai", PolicyEffect::Allow)]);
        let global = doc(vec![pol(PolicyAction::ProviderUse, "openai", PolicyEffect::Deny)]);
        // entries are lowest priority first: global then project
        let entries = vec![global, project];
        let state = ManagedState::default();
        let s = statements(&entries, &state);
        assert_eq!(s[0].effect, PolicyEffect::Allow);
        assert_eq!(s[1].effect, PolicyEffect::Deny);
        assert_eq!(s[1].message, "Blocked by configuration policy");
        // global (reversed last) outranks project
        assert!(!provider_allowed(&s, "openai"));

        let org = ManagedState {
            statements: vec![pol(PolicyAction::IntegrationUse, "mcp:*", PolicyEffect::Deny)],
            organization: Some("Acme".into()),
        };
        let s = statements(&[], &org);
        assert_eq!(s[0].message, "Blocked by Acme's policy");
        assert!(!mcp_allowed(&s, "github"));
        assert!(skill_allowed(&s, "x"));
        let s = statements(&[], &ManagedState { organization: None, ..org });
        assert_eq!(s[0].message, "Blocked by your organization's policy");
    }

    #[test]
    fn tool_use_last_match_wins() {
        let entries = vec![doc(vec![
            pol(PolicyAction::ToolUse, "shell:*", PolicyEffect::Deny),
            pol(PolicyAction::ToolUse, "shell:git *", PolicyEffect::Allow),
            pol(PolicyAction::ToolUse, "shell:git push *", PolicyEffect::Deny),
        ])];
        let s = statements(&entries, &ManagedState::default());
        assert!(tool_denial(&s, "shell", &["git status".into()]).is_none());
        assert!(tool_denial(&s, "shell", &["git push origin".into()]).is_some());
        assert!(tool_denial(&s, "shell", &["ls".into()]).is_some());
        assert_eq!(decision(&s, PolicyAction::ToolUse, "read:x"), PolicyEffect::Allow);
    }

    #[test]
    fn update_policy() {
        assert_eq!(decode_update_policy(r#"{"update":"auto"}"#), Some(UpdatePolicy::Auto));
        assert_eq!(decode_update_policy(r#"{"update":"bogus","autoupdate":true}"#), None);
        assert_eq!(decode_update_policy(r#"{"autoupdate":false}"#), Some(UpdatePolicy::Disable));
        assert_eq!(decode_update_policy(r#"{"autoupdate":"notify"}"#), Some(UpdatePolicy::Notify));
        assert_eq!(decode_update_policy(r#"{"autoupdate":true, // c
        }"#), Some(UpdatePolicy::Auto));
        assert_eq!(decode_update_policy("{}"), None);
        assert_eq!(decode_update_policy("nope"), None);
    }
}
