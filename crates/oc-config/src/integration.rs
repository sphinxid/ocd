//! Config documents delivered by integration connections (`packages/core/src/config/integration.ts`), such as the
//! OpenCode Console's workspace config. Config loads one right after managed policies, so user and project config
//! override its settings while its policies outrank theirs. Its providers authenticate with, and only exist for,
//! that connection.

use crate::decode::Decode;
use crate::schema::{Entry, Info, IntegrationSource, Mcp, McpServer};
use serde_json::{Map, Value};

const POLICY_ACTIONS: &[&str] = &["provider.use", "tool.use", "integration.use"];
const POLICY_EFFECTS: &[&str] = &["allow", "deny"];

/// `ConfigIntegration.Policies`: statements whose action or effect is a value this version does not know are
/// dropped, so a newer remote source can extend either enum. Malformed statements still fail validation.
fn known_policy(policy: &Value) -> bool {
    if let Some(Value::String(action)) = policy.get("action")
        && !POLICY_ACTIONS.contains(&action.as_str())
    {
        return false;
    }
    match policy.get("effect") {
        Some(Value::String(effect)) => POLICY_EFFECTS.contains(&effect.as_str()),
        _ => true,
    }
}

/// `ConfigIntegration.Remote`: only the allow-listed keys of a remote config (`providers`, `mcp`, `policies`,
/// `websearch`) are read before decoding, so a newer remote source's other keys, valid or not, can never reject
/// the document.
pub fn decode_remote(body: &Value) -> Result<Info, String> {
    let o = body.as_object().ok_or("Expected an object")?;
    let mut remote = Map::new();
    for key in ["providers", "mcp", "policies", "websearch"] {
        let Some(value) = o.get(key) else { continue };
        let value = if key == "policies" {
            let items = value.as_array().ok_or("Expected an array at [\"policies\"]")?;
            Value::Array(items.iter().filter(|p| known_policy(p)).cloned().collect())
        } else {
            value.clone()
        };
        remote.insert(key.into(), value);
    }
    Info::decode(&Value::Object(remote)).ok_or_else(|| "Invalid remote config".to_string())
}

fn without_credentials(rest: &mut crate::schema::JsonRecord) {
    for key in ["apiKey", "authToken", "accessToken"] {
        rest.shift_remove(key);
    }
}

/// `ConfigIntegration.make`: an integration document from a decoded remote config. Providers lose `env` (the
/// connection supplies credentials; `env` would read local variables as their key) and every `apiKey`,
/// `authToken` and `accessToken` setting; only remote MCP servers are kept (local ones would run a command on this
/// machine). A remote source can never load plugins, run commands, or change other local behavior through config.
pub fn make(integration_id: &str, connection: Value, organization: Option<String>, remote: Info) -> Entry {
    let providers = remote.providers.map(|providers| {
        providers
            .into_iter()
            .map(|(id, mut provider)| {
                provider.env = None;
                if let Some(settings) = provider.settings.as_mut() {
                    without_credentials(&mut settings.rest);
                }
                if let Some(models) = provider.models.as_mut() {
                    for model in models.values_mut() {
                        if let Some(settings) = model.settings.as_mut() {
                            without_credentials(&mut settings.rest);
                        }
                        for variant in model.variants.iter_mut().flatten() {
                            if let Some(settings) = variant.settings.as_mut() {
                                without_credentials(&mut settings.rest);
                            }
                        }
                    }
                }
                (id, provider)
            })
            .collect()
    });
    let mcp = remote.mcp.and_then(|mcp| mcp.servers).map(|servers| Mcp {
        timeout: None,
        servers: Some(servers.into_iter().filter(|(_, server)| matches!(server, McpServer::Remote(_))).collect()),
    });
    Entry::Document {
        path: None,
        info: Info { providers, mcp, policies: remote.policies, websearch: remote.websearch, ..Default::default() },
        integration: Some(IntegrationSource { integration_id: integration_id.into(), connection, organization }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn reads_only_allow_listed_keys_and_drops_unknown_policies() {
        let body = json!({
            "plugins": ["evil"],
            "shell": "rm -rf",
            "future": {"anything": true},
            "policies": [
                {"action": "provider.use", "resource": "openai", "effect": "deny"},
                {"action": "model.use", "resource": "*", "effect": "deny"},
                {"action": "tool.use", "resource": "shell:*", "effect": "warn"},
            ],
            "websearch": {"provider": "opencode"},
        });
        let info = decode_remote(&body).unwrap();
        assert!(info.plugins.is_none() && info.shell.is_none());
        let policies = info.policies.unwrap();
        assert_eq!(policies.len(), 1);
        assert_eq!(policies[0].resource, "openai");
        assert!(decode_remote(&json!({"policies": [{"action": "provider.use", "effect": "deny"}]})).is_err());
        assert!(decode_remote(&json!({"providers": "x"})).is_err());
    }

    #[test]
    fn makes_a_document_without_credentials_env_or_local_servers() {
        let remote = decode_remote(&json!({
            "providers": {"acme": {
                "env": ["ACME_KEY"],
                "settings": {"apiKey": "k", "authToken": "t", "accessToken": "a", "baseURL": "https://acme.test"},
                "models": {"m": {"settings": {"apiKey": "k", "x": 1}, "variants": [{"id": "high", "settings": {"accessToken": "a", "y": 2}}]}}
            }},
            "mcp": {"timeout": {"startup": 5000}, "servers": {
                "docs": {"type": "remote", "url": "https://mcp.acme.test"},
                "shell": {"type": "local", "command": ["sh"]}
            }}
        }))
        .unwrap();
        let connection = json!({"type": "credential", "id": "crd_1", "label": "Acme", "method": "oauth"});
        let entry = make("opencode", connection, Some("Acme".into()), remote);
        let info = entry.info().unwrap().to_value();
        assert_eq!(
            info,
            json!({
                "mcp": {"servers": {"docs": {"type": "remote", "url": "https://mcp.acme.test"}}},
                "providers": {"acme": {
                    "settings": {"baseURL": "https://acme.test"},
                    "models": {"m": {"settings": {"x": 1}, "variants": [{"id": "high", "settings": {"y": 2}}]}}
                }}
            })
        );
        let source = entry.integration().unwrap();
        assert_eq!(source.connection_key().as_deref(), Some("credential:crd_1"));
        assert_eq!(source.organization.as_deref(), Some("Acme"));
        let encoded = serde_json::to_value(&entry).unwrap();
        assert_eq!(encoded["type"], "document");
        assert_eq!(encoded["integrationID"], "opencode");
        let decoded: Entry = serde_json::from_value(encoded).unwrap();
        assert_eq!(decoded, entry);
    }
}
