//! Managed policy evaluation (`core/src/managed-policy.ts`, `config/plugin/policy.ts`), wildcard
//! matching (`core/src/util/wildcard.ts`) and the CLI updater `update` policy reader
//! (`cli/src/services/updater.ts`).

use std::path::Path;

use serde::Serialize;

use crate::schema::{Entry, PolicyAction, PolicyEffect, UpdatePolicy};

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

/// A policy statement tagged with its denial message.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Statement {
    pub action: PolicyAction,
    pub resource: String,
    pub effect: PolicyEffect,
    pub message: String,
}

pub const CONFIGURATION_POLICY_MESSAGE: &str = "Blocked by configuration policy";

/// The denial message of an organization's policies (`blocked(organization)`).
pub fn organization_message(organization: Option<&str>) -> String {
    match organization {
        Some(org) if !org.is_empty() => format!("Blocked by {org}'s policy"),
        _ => "Blocked by your organization's policy".to_string(),
    }
}

/// `ManagedPolicy.statements`: every policy statement in config, earliest-loaded first so it wins (managed policy,
/// then a Console config document, then user and project files), each with the message a denial shows. An
/// integration document's policies are those of the organization behind its connection.
pub fn statements(entries: &[Entry]) -> Vec<Statement> {
    entries
        .iter()
        .rev()
        .filter(|e| e.info().is_some())
        .flat_map(|entry| {
            let message = match entry.integration() {
                Some(source) => organization_message(source.organization.as_deref()),
                None => CONFIGURATION_POLICY_MESSAGE.to_string(),
            };
            let info = entry.info().unwrap();
            info.policies.iter().flatten().map(move |p| Statement {
                action: p.action,
                resource: p.resource.clone(),
                effect: p.effect,
                message: message.clone(),
            })
        })
        .collect()
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

/// `mcp.use` against the server name.
pub fn mcp_allowed(policies: &[Statement], name: &str) -> bool {
    decision(policies, PolicyAction::McpUse, name) != PolicyEffect::Deny
}

/// `skill.use` against the skill id.
pub fn skill_allowed(policies: &[Statement], id: &str) -> bool {
    decision(policies, PolicyAction::SkillUse, id) != PolicyEffect::Deny
}

/// `plugin.use` against a plugin operation target (`PluginModule.make`): the target without its `@version`, unless
/// it is an absolute path.
pub fn plugin_allowed(policies: &[Statement], target: &str) -> bool {
    decision(policies, PolicyAction::PluginUse, &plugin_policy_target(target)) != PolicyEffect::Deny
}

/// The resource `plugin.use` matches: `target` with a trailing `@version` removed (`lastIndexOf("@") > 0`), kept
/// whole when it is an absolute path.
pub fn plugin_policy_target(target: &str) -> String {
    let version = target.rfind('@').filter(|&i| i > 0);
    match version {
        Some(i) if !Path::new(target).is_absolute() => target[..i].to_string(),
        _ => target.to_string(),
    }
}

/// `ManagedPolicy.integrationResource`: `<integration>#<account>` when a connection with a known account backs what
/// the integration supplies, otherwise the bare integration id (env keys, local config, the signed-out free tier).
pub fn integration_resource(integration_id: &str, account: Option<&str>) -> String {
    match account {
        Some(account) if !account.is_empty() => format!("{integration_id}#{account}"),
        _ => integration_id.to_string(),
    }
}

/// `ManagedPolicy.account`: the organization, or failing that the account, a stored credential's metadata names.
pub fn account(metadata: Option<&serde_json::Map<String, serde_json::Value>>) -> Option<String> {
    let metadata = metadata?;
    ["orgID", "accountID"]
        .iter()
        .find_map(|k| metadata.get(*k).and_then(serde_json::Value::as_str).filter(|v| !v.is_empty()))
        .map(String::from)
}

/// `integration.use` denies what integration `integration_id` supplies through `account`.
pub fn integration_denied(policies: &[Statement], integration_id: &str, account: Option<&str>) -> bool {
    decision(policies, PolicyAction::IntegrationUse, &integration_resource(integration_id, account)) == PolicyEffect::Deny
}

/// `integration.connect`: the statement denying that integration `id` is listed or connected, if any.
pub fn connect_denial<'a>(policies: &'a [Statement], id: &str) -> Option<&'a Statement> {
    find(policies, PolicyAction::IntegrationConnect, id).filter(|p| p.effect == PolicyEffect::Deny)
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
    use crate::decode::Decode;
    use crate::schema::{Info, Policy};

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
        Entry::document(None, Info { policies: Some(policies), ..Default::default() })
    }

    fn console(policies: Vec<Policy>, organization: Option<&str>) -> Entry {
        Entry::Document {
            path: None,
            info: Info { policies: Some(policies), ..Default::default() },
            integration: Some(crate::schema::IntegrationSource {
                integration_id: "opencode".into(),
                connection: serde_json::json!({"type": "credential", "id": "crd_1", "label": "Acme", "method": "oauth"}),
                organization: organization.map(String::from),
                account: None,
            }),
            wellknown: None,
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
        let s = statements(&entries);
        assert_eq!(s[0].effect, PolicyEffect::Allow);
        assert_eq!(s[1].effect, PolicyEffect::Deny);
        assert_eq!(s[1].message, "Blocked by configuration policy");
        // global (reversed last) outranks project
        assert!(!provider_allowed(&s, "openai"));

        // A Console document loads right after managed policies, so its policies outrank user and project ones.
        let org = console(vec![pol(PolicyAction::McpUse, "*", PolicyEffect::Deny)], Some("Acme"));
        let user = doc(vec![pol(PolicyAction::McpUse, "github", PolicyEffect::Allow)]);
        let s = statements(&[org.clone(), user]);
        assert_eq!(s[1].message, "Blocked by Acme's policy");
        assert!(!mcp_allowed(&s, "github"));
        assert!(skill_allowed(&s, "x"));
        let s = statements(&[console(vec![pol(PolicyAction::McpUse, "*", PolicyEffect::Deny)], None)]);
        assert_eq!(s[0].message, "Blocked by your organization's policy");
    }

    #[test]
    fn tool_use_last_match_wins() {
        let entries = vec![doc(vec![
            pol(PolicyAction::ToolUse, "shell:*", PolicyEffect::Deny),
            pol(PolicyAction::ToolUse, "shell:git *", PolicyEffect::Allow),
            pol(PolicyAction::ToolUse, "shell:git push *", PolicyEffect::Deny),
        ])];
        let s = statements(&entries);
        assert!(tool_denial(&s, "shell", &["git status".into()]).is_none());
        assert!(tool_denial(&s, "shell", &["git push origin".into()]).is_some());
        assert!(tool_denial(&s, "shell", &["ls".into()]).is_some());
        assert_eq!(decision(&s, PolicyAction::ToolUse, "read:x"), PolicyEffect::Allow);
    }

    #[test]
    fn plugin_targets_drop_their_version_unless_absolute() {
        assert_eq!(plugin_policy_target("opencode-foo@1.2.3"), "opencode-foo");
        assert_eq!(plugin_policy_target("@scope/pkg@2"), "@scope/pkg");
        assert_eq!(plugin_policy_target("@scope/pkg"), "@scope/pkg");
        assert_eq!(plugin_policy_target("plain"), "plain");
        assert_eq!(plugin_policy_target("/abs/dir@v2/plugin.ts"), "/abs/dir@v2/plugin.ts");
        assert_eq!(plugin_policy_target("file:///x/p@1"), "file:///x/p");
        let s = statements(&[doc(vec![pol(PolicyAction::PluginUse, "opencode-*", PolicyEffect::Deny)])]);
        assert!(!plugin_allowed(&s, "opencode-foo@1.2.3"));
        assert!(plugin_allowed(&s, "other@1"));
    }

    #[test]
    fn integration_resources_name_the_account() {
        assert_eq!(integration_resource("opencode", Some("org_1")), "opencode#org_1");
        assert_eq!(integration_resource("opencode", None), "opencode");
        assert_eq!(integration_resource("opencode", Some("")), "opencode");
        let meta = |v: serde_json::Value| v.as_object().cloned();
        assert_eq!(account(meta(serde_json::json!({"orgID": "o", "accountID": "a"})).as_ref()), Some("o".into()));
        assert_eq!(account(meta(serde_json::json!({"orgID": "", "accountID": "a"})).as_ref()), Some("a".into()));
        assert_eq!(account(meta(serde_json::json!({"orgID": 5})).as_ref()), None);
        assert_eq!(account(None), None);
        // A bare pattern only matches connectionless use; `opencode#*` every account.
        let s = statements(&[doc(vec![pol(PolicyAction::IntegrationUse, "opencode", PolicyEffect::Deny)])]);
        assert!(integration_denied(&s, "opencode", None));
        assert!(!integration_denied(&s, "opencode", Some("org_1")));
        let s = statements(&[doc(vec![pol(PolicyAction::IntegrationUse, "opencode#*", PolicyEffect::Deny)])]);
        assert!(integration_denied(&s, "opencode", Some("org_1")));
        assert!(!integration_denied(&s, "opencode", None));
    }

    #[test]
    fn legacy_integration_use_resources_convert() {
        let decode = |v: serde_json::Value| Policy::decode(&v).unwrap();
        let p = decode(serde_json::json!({"action": "integration.use", "resource": "mcp:github", "effect": "deny"}));
        assert_eq!((p.action, p.resource.as_str()), (PolicyAction::McpUse, "github"));
        let p = decode(serde_json::json!({"action": "integration.use", "resource": "skill:a:b", "effect": "deny"}));
        assert_eq!((p.action, p.resource.as_str()), (PolicyAction::SkillUse, "a:b"));
        let p = decode(serde_json::json!({"action": "integration.use", "resource": "plugin:*", "effect": "allow"}));
        assert_eq!((p.action, p.resource.as_str()), (PolicyAction::PluginUse, "*"));
        let p = decode(serde_json::json!({"action": "integration.use", "resource": "*", "effect": "deny"}));
        assert_eq!((p.action, p.resource.as_str()), (PolicyAction::IntegrationUse, "*"));
        let p = decode(serde_json::json!({"action": "integration.use", "resource": "https://x.dev:8080", "effect": "deny"}));
        assert_eq!((p.action, p.resource.as_str()), (PolicyAction::IntegrationUse, "https://x.dev:8080"));
        let p = decode(serde_json::json!({"action": "tool.use", "resource": "mcp:x", "effect": "deny"}));
        assert_eq!((p.action, p.resource.as_str()), (PolicyAction::ToolUse, "mcp:x"));
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
