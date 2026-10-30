//! MCP integration (`core/src/mcp/index.ts`, `config/plugin/mcp.ts`): one manager per location,
//! credentials in the `credential` table, tools exposed through the tool registry.

use std::sync::Arc;

use async_trait::async_trait;
use rusqlite::{params, OptionalExtension};
use serde_json::Value;

use oc_mcp::manager::{ManagerOptions, McpManager};
use oc_mcp::oauth::{McpCredentialStore, StoredCredential};
use oc_mcp::types::ClientInfo;
use oc_storage::Database;
use parking_lot::Mutex;
use std::collections::HashMap;
use tokio_util::sync::CancellationToken;

use crate::form::{Forms, State};

use crate::location::Location;

pub struct McpCreds(pub Database);

#[async_trait]
impl McpCredentialStore for McpCreds {
    async fn list(&self, integration_id: &str) -> anyhow::Result<Vec<StoredCredential>> {
        self.0.with(|c| {
            let mut s = c.prepare(
                "SELECT id, integration_id, label, value FROM credential WHERE integration_id = ? ORDER BY active ASC, time_created ASC, id ASC",
            )?;
            let r = s
                .query_map([integration_id], |r| {
                    let v: String = r.get(3)?;
                    Ok(StoredCredential { id: r.get(0)?, integration_id: r.get(1)?, label: r.get(2)?, value: serde_json::from_str(&v).unwrap_or(Value::Null) })
                })?
                .collect();
            r
        })
    }
    async fn get(&self, id: &str) -> anyhow::Result<Option<StoredCredential>> {
        self.0.with(|c| {
            c.query_row("SELECT id, integration_id, label, value FROM credential WHERE id = ?", [id], |r| {
                let v: String = r.get(3)?;
                Ok(StoredCredential { id: r.get(0)?, integration_id: r.get(1)?, label: r.get(2)?, value: serde_json::from_str(&v).unwrap_or(Value::Null) })
            })
            .optional()
        })
    }
    async fn create(&self, integration_id: &str, label: &str, value: Value) -> anyhow::Result<String> {
        let id = oc_schema::id::credential();
        let now = oc_schema::id::now_ms() as i64;
        self.0.transaction(|c| {
            c.execute("UPDATE credential SET active = 0 WHERE integration_id = ?", [integration_id])?;
            c.execute(
                "INSERT INTO credential (id, integration_id, label, value, active, time_created, time_updated) VALUES (?, ?, ?, ?, 1, ?, ?)",
                params![id, integration_id, label, value.to_string(), now, now],
            )?;
            Ok(())
        })?;
        Ok(id)
    }
    async fn update(&self, id: &str, value: Value) -> anyhow::Result<()> {
        self.0.with(|c| {
            c.execute(
                "UPDATE credential SET value = ?, time_updated = ? WHERE id = ?",
                params![value.to_string(), oc_schema::id::now_ms() as i64, id],
            )
        })?;
        Ok(())
    }
    async fn remove(&self, id: &str) -> anyhow::Result<()> {
        self.0.with(|c| c.execute("DELETE FROM credential WHERE id = ?", [id]))?;
        Ok(())
    }
}

/// Config `mcp` sections from every document, in order.
pub fn sections(location: &Location) -> Vec<oc_mcp::config::McpSection> {
    location
        .entries
        .iter()
        .filter_map(|e| match e {
            oc_config::schema::Entry::Document { info, .. } => info.mcp.as_ref(),
            _ => None,
        })
        .filter_map(|m| serde_json::to_value(m).ok().and_then(|v| serde_json::from_value(v).ok()))
        .collect()
}

/// The location's MCP servers: configuration (`opencode.config.mcp`, a `pre` plugin), then plugin
/// transforms.
pub async fn servers(plugins: &crate::plugins::Plugins, location: &Location) -> indexmap::IndexMap<String, oc_mcp::config::ServerConfig> {
    let s = sections(location);
    plugins.mcp_servers(location, oc_mcp::config::merge_documents(s.iter())).await
}

/// The location's MCP manager. `sink` receives its events from the first one on (the startup statuses
/// included), before the servers are set.
pub async fn manager(
    db: &Database,
    forms: &Forms,
    location: &Location,
    version: &str,
    plugins: &crate::plugins::Plugins,
    sink: oc_mcp::manager::EventSink,
) -> Arc<McpManager> {
    let manager = Arc::new(McpManager::new(ManagerOptions {
        directory: location.directory.clone(),
        client_info: ClientInfo { name: "opencode".into(), version: version.into() },
        credentials: Some(Arc::new(McpCreds(db.clone()))),
        elicitation: Some(Arc::new(Elicitation::new(
            forms.clone(),
            Some(oc_schema::types::LocationRef { directory: location.key.clone(), workspace_id: None }),
        ))),
    }));
    manager.on_event(sink);
    manager.set_servers(servers(plugins, location).await).await;
    manager
}

/// The OAuth login method of a remote MCP server (`register` in `core/src/mcp/index.ts`): the
/// integration's `oauth` method runs the server's MCP OAuth flow.
struct McpOAuthMethod {
    integration_id: String,
    name: String,
    manager: Arc<McpManager>,
}

#[async_trait]
impl oc_ai::credentials::integration::OAuthMethod for McpOAuthMethod {
    fn integration_id(&self) -> String {
        self.integration_id.clone()
    }

    fn info(&self) -> oc_ai::credentials::integration::OAuthMethodInfo {
        oc_ai::credentials::integration::OAuthMethodInfo::new(&self.integration_id, &self.name)
    }

    async fn authorize(
        &self,
        _answer: oc_ai::credentials::form::FormAnswer,
        scope: CancellationToken,
    ) -> oc_ai::credentials::integration::FlowResult<oc_ai::credentials::integration::AuthorizeResult> {
        use oc_ai::credentials::integration::{AuthorizeResult, FlowError};
        let handle = self.manager.auth_start(&self.name).await.map_err(|e| FlowError::new(e.to_string()))?;
        // The callback server stops with the attempt's scope.
        let cancel = handle.canceller();
        tokio::spawn(async move {
            tokio::select! {
                _ = scope.cancelled() => cancel.cancel(),
                _ = cancel.cancelled() => {}
            }
        });
        let (url, instructions) = (handle.url.clone(), handle.instructions.clone());
        Ok(AuthorizeResult::auto(url, instructions, async move {
            let value = handle.wait().await.map_err(FlowError::new)?;
            let text = |k: &str| value.get(k).and_then(Value::as_str).unwrap_or("").to_string();
            Ok(oc_ai::credentials::OAuthValue {
                method_id: text("methodID"),
                refresh: text("refresh"),
                access: text("access"),
                expires: value.get("expires").and_then(Value::as_u64).unwrap_or(0),
                metadata: value.get("metadata").and_then(Value::as_object).cloned(),
            })
        }))
    }
}

/// Registers every remote OAuth-capable server of `manager` as an integration (`mcp_<hash>`, named
/// after the server, `metadata.source: "mcp"`, one `oauth` method labelled with the server name),
/// and drops MCP integrations whose server is gone. Unchanged registrations are left alone.
pub fn register_integrations(integrations: &oc_ai::credentials::integration::Integrations, manager: &Arc<McpManager>) {
    use oc_ai::credentials::integration::Method;
    let is_mcp = |metadata: &Option<oc_ai::credentials::Metadata>| metadata.as_ref().and_then(|m| m.get("source")).and_then(Value::as_str) == Some("mcp");
    let servers: Vec<(String, String)> = manager.servers().into_iter().filter_map(|s| s.integration_id.map(|id| (id, s.name))).collect();
    let refs = integrations.refs();
    for r in refs.iter().filter(|r| is_mcp(&r.metadata)) {
        if !servers.iter().any(|(id, _)| *id == r.id) {
            integrations.remove(&r.id);
        }
    }
    for (id, name) in servers {
        let registered = refs.iter().any(|r| r.id == id && r.name == name && is_mcp(&r.metadata))
            && integrations.methods(&id).iter().any(|m| matches!(m, Method::Oauth { id: mid, label, .. } if *mid == id && *label == name));
        if registered {
            continue;
        }
        integrations.update(&id, |r| {
            r.name = name.clone();
            r.metadata = Some(serde_json::Map::from_iter([("source".to_string(), Value::String("mcp".into()))]));
        });
        integrations.register_oauth(Arc::new(McpOAuthMethod { integration_id: id, name, manager: manager.clone() }));
    }
}

/// Sentinel owner session of MCP elicitation forms.
pub const GLOBAL_ELICITATION_SESSION_ID: &str = "global";
pub const URL_ELICITATION_FIELD_KEY: &str = "elicitation";

/// `elicitation/create` → Forms (`core/src/mcp/index.ts` `elicitation`).
pub struct Elicitation {
    forms: Forms,
    /// The server's Location: elicitation forms are published in it. Clients drop unlocated
    /// `form.created` events, and the elicitation then hangs (889abb465).
    location: Option<oc_schema::types::LocationRef>,
    /// `server\0elicitationId` → form id (legacy URL-mode completion).
    urls: Arc<Mutex<HashMap<String, String>>>,
}

impl Elicitation {
    pub fn new(forms: Forms, location: Option<oc_schema::types::LocationRef>) -> Self {
        Elicitation { forms, location, urls: Arc::default() }
    }

    async fn ask(&self, input: Value, cancel: CancellationToken) -> Result<State, String> {
        tokio::select! {
            state = self.forms.ask_at(&input, self.location.clone()) => state.map_err(|e| e.to_string()),
            _ = cancel.cancelled() => Ok(State::Cancelled(None)),
        }
    }
}

#[async_trait]
impl oc_mcp::ElicitationHandler for Elicitation {
    async fn create(&self, server: &str, params: Value, cancel: CancellationToken) -> Result<Value, String> {
        let title = format!("{server} is requesting input");
        let message = params.get("message").cloned().unwrap_or(Value::Null);
        if params.get("mode").and_then(Value::as_str) == Some("url") {
            let id = oc_schema::id::form();
            // Legacy only: newer protocol versions have no elicitationId and no completion notification.
            let elicitation_id = params.get("elicitationId").and_then(Value::as_str).map(str::to_string);
            let key = elicitation_id.as_ref().map(|e| format!("{server}\u{0}{e}"));
            if let Some(key) = &key {
                self.urls.lock().insert(key.clone(), id.clone());
            }
            let mut metadata = serde_json::json!({"kind": "mcp-elicitation", "server": server});
            if let Some(e) = &elicitation_id {
                metadata["elicitationID"] = serde_json::json!(e);
            }
            metadata["message"] = message;
            let input = serde_json::json!({
                "id": id,
                "sessionID": GLOBAL_ELICITATION_SESSION_ID,
                "title": title,
                "metadata": metadata,
                "fields": [{"key": URL_ELICITATION_FIELD_KEY, "type": "external", "url": params.get("url").cloned().unwrap_or(Value::Null)}],
            });
            let state = self.ask(input, cancel).await;
            if let Some(key) = &key {
                self.urls.lock().remove(key);
            }
            let action = if matches!(state?, State::Answered(_)) { "accept" } else { "cancel" };
            return Ok(serde_json::json!({"action": action}));
        }
        let schema = params.get("requestedSchema").cloned().unwrap_or(Value::Null);
        let required: Vec<&str> =
            schema.get("required").and_then(Value::as_array).map(|r| r.iter().filter_map(Value::as_str).collect()).unwrap_or_default();
        let fields: Vec<Value> = schema
            .get("properties")
            .and_then(Value::as_object)
            .map(|p| p.iter().map(|(k, v)| elicitation_field(k, v, required.contains(&k.as_str()))).collect())
            .unwrap_or_default();
        if fields.is_empty() {
            return Ok(serde_json::json!({"action": "accept", "content": {}}));
        }
        let input = serde_json::json!({
            "sessionID": GLOBAL_ELICITATION_SESSION_ID,
            "title": title,
            "metadata": {"kind": "mcp-elicitation", "server": server, "message": message},
            "fields": fields,
        });
        match self.ask(input, cancel).await? {
            State::Answered(answer) => Ok(serde_json::json!({"action": "accept", "content": answer})),
            _ => Ok(serde_json::json!({"action": "cancel"})),
        }
    }

    async fn complete(&self, server: &str, elicitation_id: &str) {
        let id = self.urls.lock().get(&format!("{server}\u{0}{elicitation_id}")).cloned();
        if let Some(id) = id {
            let mut answer = serde_json::Map::new();
            answer.insert(URL_ELICITATION_FIELD_KEY.into(), Value::Bool(true));
            let _ = self.forms.reply(&id, answer);
        }
    }
}

/// `toElicitationField`: one requested-schema property → Form field. `None`-valued keys are omitted.
pub fn elicitation_field(key: &str, property: &Value, required: bool) -> Value {
    static MACHINE_TITLE: std::sync::LazyLock<regex::Regex> = std::sync::LazyLock::new(|| {
        regex::Regex::new(r"(?i)^(boolean|string|number|integer|array|object)(\s+with\b.*|\s+in\b.*)?$").unwrap()
    });
    let description = property.get("description").and_then(Value::as_str);
    let title = match property.get("title").and_then(Value::as_str) {
        Some(t) if !t.is_empty() && !MACHINE_TITLE.is_match(t.trim()) => t.to_string(),
        _ => description.unwrap_or(key).to_string(),
    };
    let mut field = serde_json::Map::new();
    let mut put = |k: &str, v: Option<Value>| {
        if let Some(v) = v.filter(|v| !v.is_null()) {
            field.insert(k.into(), v);
        }
    };
    let get = |k: &str| property.get(k).cloned();
    put("key", Some(Value::from(key)));
    put("title", Some(Value::from(title.clone())));
    put("description", description.filter(|d| *d != title).map(Value::from));
    put("required", required.then_some(Value::Bool(true)));
    match property.get("type").and_then(Value::as_str).unwrap_or_default() {
        "boolean" => {
            put("type", Some("boolean".into()));
            put("default", get("default"));
        }
        t @ ("number" | "integer") => {
            put("type", Some(t.into()));
            put("minimum", get("minimum"));
            put("maximum", get("maximum"));
            put("default", get("default"));
        }
        "array" => {
            let items = property.get("items").cloned().unwrap_or(Value::Null);
            let options: Vec<Value> = if let Some(any) = items.get("anyOf").and_then(Value::as_array) {
                any.iter().map(|o| option(o.get("const"), o.get("title"))).collect()
            } else {
                items.get("enum").and_then(Value::as_array).map(|e| e.iter().map(|v| option(Some(v), Some(v))).collect()).unwrap_or_default()
            };
            put("type", Some("multiselect".into()));
            put("options", Some(Value::Array(options)));
            put("custom", Some(Value::Bool(false)));
            put("minItems", get("minItems"));
            put("maxItems", get("maxItems"));
            put("default", get("default"));
        }
        _ => {
            let options: Option<Vec<Value>> = if let Some(one) = property.get("oneOf").and_then(Value::as_array) {
                Some(one.iter().map(|o| option(o.get("const"), o.get("title"))).collect())
            } else {
                property.get("enum").and_then(Value::as_array).map(|e| {
                    let names = property.get("enumNames").and_then(Value::as_array);
                    e.iter()
                        .enumerate()
                        .map(|(i, v)| option(Some(v), names.and_then(|n| n.get(i)).filter(|n| !n.is_null()).or(Some(v))))
                        .collect()
                })
            };
            let custom = options.as_ref().map(|_| Value::Bool(false));
            put("type", Some("string".into()));
            put("format", get("format"));
            put("minLength", get("minLength"));
            put("maxLength", get("maxLength"));
            put("default", get("default"));
            put("options", options.map(Value::Array));
            put("custom", custom);
        }
    }
    Value::Object(field)
}

fn option(value: Option<&Value>, label: Option<&Value>) -> Value {
    let mut o = serde_json::Map::new();
    o.insert("value".into(), value.cloned().unwrap_or(Value::Null));
    if let Some(l) = label.filter(|l| !l.is_null()) {
        o.insert("label".into(), l.clone());
    }
    Value::Object(o)
}

#[cfg(test)]
mod elicitation_tests {
    use super::*;
    use serde_json::json;

    /// `register` in `mcp/index.ts`: remote servers become `mcp_<hash>` OAuth integrations (recorded
    /// from upstream 2.1.4's `integration.list`), `oauth: false` and local servers don't, and
    /// removed servers drop out.
    #[tokio::test]
    async fn remote_servers_register_oauth_integrations() {
        let integrations = oc_ai::credentials::integration::Integrations::new(oc_ai::credentials::store::MemoryCredentialStore::shared());
        let manager = Arc::new(McpManager::new(ManagerOptions {
            directory: std::env::temp_dir(),
            client_info: ClientInfo { name: "opencode".into(), version: "test".into() },
            credentials: None,
            elicitation: None,
        }));
        let servers = |v: serde_json::Value| serde_json::from_value::<indexmap::IndexMap<String, oc_mcp::config::ServerConfig>>(v).unwrap();
        manager
            .set_servers(servers(json!({
                "alpha": {"type": "remote", "url": "http://127.0.0.1:9/alpha", "disabled": true},
                "plain": {"type": "remote", "url": "http://127.0.0.1:9/plain", "oauth": false, "disabled": true},
                "local": {"type": "local", "command": ["true"], "disabled": true},
            })))
            .await;
        register_integrations(&integrations, &manager);
        let list = integrations.list().await.unwrap();
        let mcp: Vec<serde_json::Value> = list.iter().filter(|i| i.id.starts_with("mcp_")).map(|i| serde_json::to_value(i).unwrap()).collect();
        assert_eq!(
            mcp,
            vec![json!({"id": "mcp_ad7fcff5e94db8a6", "name": "alpha", "metadata": {"source": "mcp"}, "methods": [{"id": "mcp_ad7fcff5e94db8a6", "type": "oauth", "label": "alpha"}], "connections": []})]
        );
        let revision = integrations.revision();
        register_integrations(&integrations, &manager);
        assert_eq!(integrations.revision(), revision, "unchanged registrations are not re-published");
        manager.set_servers(servers(json!({}))).await;
        register_integrations(&integrations, &manager);
        assert!(!integrations.list().await.unwrap().iter().any(|i| i.id.starts_with("mcp_")));
    }

    #[test]
    fn machine_titles_fall_back() {
        let f = elicitation_field("email", &json!({"type": "string", "title": "string with format email", "format": "email"}), true);
        assert_eq!(f, json!({"key": "email", "title": "email", "required": true, "type": "string", "format": "email"}));
        let f = elicitation_field("n", &json!({"type": "integer", "title": "Count", "description": "How many", "minimum": 1}), false);
        assert_eq!(f, json!({"key": "n", "title": "Count", "description": "How many", "type": "integer", "minimum": 1}));
    }

    // mcp.test.ts: clients drop `form.created` events without a Location, so the form must be
    // published inside the server's Location (889abb465).
    #[tokio::test]
    async fn elicitation_forms_are_published_in_the_server_location() {
        use oc_mcp::ElicitationHandler;
        let bus = crate::bus::Bus::new(oc_storage::Database::memory().unwrap());
        let forms = Forms::new(bus.clone());
        let mut events = bus.subscribe();
        let location = oc_schema::types::LocationRef { directory: "/project".into(), workspace_id: None };
        let elicitation = Arc::new(Elicitation::new(forms.clone(), Some(location.clone())));
        let params = json!({"mode": "form", "message": "Name?", "requestedSchema": {"type": "object", "properties": {"name": {"type": "string"}}}});
        let pending = tokio::spawn({
            let elicitation = elicitation.clone();
            async move { elicitation.create("srv", params, CancellationToken::new()).await }
        });
        let created = loop {
            let event = events.recv().await.unwrap();
            if event.kind == "form.created" {
                break event;
            }
        };
        assert_eq!(created.location.as_ref(), Some(&location));
        let id = created.data["form"]["id"].as_str().unwrap().to_string();
        let mut answer = serde_json::Map::new();
        answer.insert("name".into(), json!("Ada"));
        forms.reply(&id, answer).unwrap();
        let replied = loop {
            let event = events.recv().await.unwrap();
            if event.kind == "form.replied" {
                break event;
            }
        };
        assert_eq!(replied.location.as_ref(), Some(&location));
        assert_eq!(pending.await.unwrap().unwrap(), json!({"action": "accept", "content": {"name": "Ada"}}));
    }

    #[test]
    fn enums_become_options() {
        let f = elicitation_field("c", &json!({"type": "string", "enum": ["a", "b"], "enumNames": ["A"]}), false);
        assert_eq!(f["options"], json!([{"value": "a", "label": "A"}, {"value": "b", "label": "b"}]));
        assert_eq!(f["custom"], json!(false));
        let f = elicitation_field("m", &json!({"type": "array", "items": {"anyOf": [{"const": "x", "title": "X"}]}}), false);
        assert_eq!(f["type"], "multiselect");
        assert_eq!(f["options"], json!([{"value": "x", "label": "X"}]));
    }
}
