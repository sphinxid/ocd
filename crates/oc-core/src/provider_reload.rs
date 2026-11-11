//! Which provider plugins reload their providers when their integration's active credential switches
//! (`ProviderRefresh.onSwitch` with `reloadIfChanged`, `credentialSettings`), and the state each derives from that
//! credential: a switch reloads only when it changed (`provider.updated`, `model.updated`).
//!
//! GitHub Copilot (its own list refresh), OpenCode and the local agent CLI reload on their own.

use oc_ai::catalog::transforms::CredentialLogins;
use oc_ai::credentials::{Credential, CredentialValue};
use serde_json::Value;

/// Plugin → integration, in plugin order. `credentialSettings` plugins and the plugins' own loads (GitLab has both:
/// its settings subscription is registered first).
pub const SWITCH_RELOADS: [(&str, &str); 12] = [
    ("opencode.provider.amazon.bedrock", "amazon-bedrock"),
    ("opencode.provider.azure", "azure"),
    ("opencode.provider.chatgpt", "openai"),
    ("opencode.provider.cloudflare.ai.gateway", "cloudflare-ai-gateway"),
    ("opencode.provider.cloudflare.workers.ai", "cloudflare-workers-ai"),
    ("opencode.provider.digitalocean", "digitalocean"),
    ("opencode.provider.gitlab#settings", "gitlab"),
    ("opencode.provider.gitlab", "gitlab"),
    ("opencode.provider.modal", "modal"),
    ("opencode.provider.snowflake.cortex", "snowflake-cortex"),
    ("opencode.provider.openai", "openai"),
    ("opencode.provider.openai-context-caps", "openai"),
];

/// The built-in plugin a [`SWITCH_RELOADS`] entry belongs to.
pub fn plugin_id(entry: &str) -> &str {
    entry.split('#').next().unwrap_or(entry)
}

/// The integration's active credential: its selected row (`all` in list order, so the last of the integration).
pub fn active<'a>(all: &'a [Credential], integration: &str) -> Option<&'a Credential> {
    all.iter().rev().find(|c| c.integration_id == integration)
}

fn metadata_str<'a>(value: &'a CredentialValue, key: &str) -> Option<&'a str> {
    let metadata = match value {
        CredentialValue::Key { metadata, .. } | CredentialValue::Oauth { metadata, .. } | CredentialValue::External { metadata, .. } => metadata.as_ref(),
    };
    metadata.and_then(|m| m.get(key)).and_then(Value::as_str)
}

fn oauth_method(value: &CredentialValue) -> Option<&str> {
    match value {
        CredentialValue::Oauth { method_id, .. } => Some(method_id.as_str()),
        _ => None,
    }
}

/// `normalizeAccount` (snowflake-cortex.ts).
fn normalize_account(value: Option<&str>) -> String {
    let Some(value) = value else { return String::new() };
    let mut out = value.trim().to_string();
    for scheme in ["https://", "http://"] {
        if out.len() >= scheme.len() && out[..scheme.len()].eq_ignore_ascii_case(scheme) {
            out = out[scheme.len()..].to_string();
            break;
        }
    }
    let out = out.trim_end_matches('/');
    let suffix = ".snowflakecomputing.com";
    if out.len() >= suffix.len() && out[out.len() - suffix.len()..].eq_ignore_ascii_case(suffix) {
        return out[..out.len() - suffix.len()].to_string();
    }
    out.to_string()
}

/// The state the plugin of `entry` derives from its integration's active credential (`load`'s result, compared with
/// `Equal.equals` or as JSON).
pub fn derived(entry: &str, credential: Option<&Credential>, env: &dyn Fn(&str) -> Option<String>) -> String {
    let value = credential.map(|c| &c.value);
    let connection = credential.map(|c| format!("credential:{}", c.id)).unwrap_or_default();
    let integration = SWITCH_RELOADS.iter().find(|(e, _)| *e == entry).map(|(_, i)| *i).unwrap_or_default();
    match entry {
        // `credentialSettings`: the settings selected from the stored credential.
        "opencode.provider.amazon.bedrock" | "opencode.provider.cloudflare.ai.gateway" | "opencode.provider.cloudflare.workers.ai" | "opencode.provider.gitlab#settings" => {
            let mut logins = CredentialLogins::default();
            logins.set(integration, value);
            format!("{logins:?}")
        }
        // The ChatGPT sign-in (present or not, and its account).
        "opencode.provider.openai" => match value.filter(|v| matches!(oauth_method(v), Some("chatgpt-browser" | "chatgpt-headless"))) {
            Some(v) => format!("signed-in:{}", metadata_str(v, "accountID").unwrap_or_default()),
            None => "none".into(),
        },
        // The token-sharing source connection.
        "opencode.provider.chatgpt" => match value.filter(|v| oauth_method(v) == Some("chatgpt-token-sharing")) {
            Some(_) => connection,
            None => String::new(),
        },
        "opencode.provider.openai-context-caps" => {
            let signed_in = value.is_some_and(|v| matches!(oauth_method(v), Some("chatgpt-browser" | "chatgpt-headless" | "chatgpt-token-sharing")));
            signed_in.to_string()
        }
        // The connection (its model list is not discovered here).
        "opencode.provider.gitlab" => match value {
            Some(CredentialValue::External { .. }) | None => String::new(),
            Some(_) => connection,
        },
        "opencode.provider.snowflake.cortex" => {
            normalize_account(env("SNOWFLAKE_ACCOUNT").as_deref().or_else(|| value.and_then(|v| metadata_str(v, "account"))))
        }
        // `rebind`: the connection and its resource.
        "opencode.provider.azure" => {
            let resource = value
                .filter(|v| matches!(v, CredentialValue::Key { .. }) || matches!(v, CredentialValue::External { method_id, .. } if method_id == "azure-cli"))
                .and_then(|v| metadata_str(v, "resourceName"))
                .filter(|r| !r.trim().is_empty())
                .unwrap_or_default();
            format!("{connection}|{resource}")
        }
        // The routers' cache key of a browser sign-in.
        "opencode.provider.digitalocean" => match value.filter(|v| oauth_method(v) == Some("browser")) {
            Some(_) => format!("digitalocean:{connection}"),
            None => String::new(),
        },
        // The connection of a key (the model list is not discovered here).
        "opencode.provider.modal" => match value {
            Some(CredentialValue::Key { .. }) => connection,
            _ => String::new(),
        },
        _ => String::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn credential(id: &str, integration: &str, value: Value) -> Credential {
        Credential { id: id.into(), integration_id: integration.into(), label: id.into(), value: serde_json::from_value(value).unwrap(), active: Some(true) }
    }

    // provider-openai.test.ts / provider-chatgpt.test.ts: reload only when the sign-in appears, disappears or
    // changes account; context caps on a `signedIn` flip.
    #[test]
    fn derived_states() {
        let env = |_: &str| None;
        let oauth = |method: &str, account: &str| {
            json!({"type": "oauth", "methodID": method, "refresh": "r", "access": "a", "expires": 0, "metadata": {"accountID": account}})
        };
        let a = credential("a", "openai", oauth("chatgpt-browser", "acct1"));
        let b = credential("b", "openai", oauth("chatgpt-headless", "acct1"));
        let c = credential("c", "openai", oauth("chatgpt-browser", "acct2"));
        let key = credential("k", "openai", json!({"type": "key", "key": "sk"}));
        let openai = |c: Option<&Credential>| derived("opencode.provider.openai", c, &env);
        assert_eq!(openai(Some(&a)), openai(Some(&b)));
        assert_ne!(openai(Some(&a)), openai(Some(&c)));
        assert_ne!(openai(Some(&a)), openai(Some(&key)));
        assert_eq!(openai(Some(&key)), openai(None));
        let caps = |c: Option<&Credential>| derived("opencode.provider.openai-context-caps", c, &env);
        assert_eq!(caps(Some(&a)), caps(Some(&c)));
        assert_ne!(caps(Some(&a)), caps(Some(&key)));
        assert_eq!(normalize_account(Some(" HTTPS://Org-Acct.snowflakecomputing.com/ ")), "Org-Acct");
        let all = vec![a.clone(), credential("x", "anthropic", json!({"type": "key", "key": "k"})), c.clone()];
        assert_eq!(active(&all, "openai").map(|c| c.id.as_str()), Some("c"));
    }
}
