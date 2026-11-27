//! The built-in Claude Agent provider (`opencode.provider.claude-agent`,
//! `packages/core/src/plugin/provider/claude-agent/`): OpenCode sessions run through the local
//! Claude Code (`claude`) over its stdio protocol, with OpenCode's tools offered as an in-process
//! MCP server.
//!
//! - **Integration** `claude-agent` with the external `claude-login` method; the login itself
//!   belongs to `claude auth login`.
//! - **Status** per location (`checking` → `missing` | `failed` | `ready`), from the login probe
//!   (on plugin start without a saved model list, and on every `credential.switched` for the
//!   integration) and from every chat process's own `initialize`. It sets the connection's
//!   `needs_auth` status and saves the model catalog (plugin KV `models`).
//! - **Provider** `claude-agent` (`aisdk:@opencode/claude-agent`, activation `auto`) with the saved
//!   models, priced from the account's API provider.
//! - **Requests**: [`bridge::ClaudeBridge`] serves the provider's `doStream` natively in front of the
//!   sidecar AI SDK bridge; the session registry ([`sessions`]) keeps one `claude` per session.
//! - **Request hooks** (`session.ts`, `instructions.ts`): see [`hooks`].
//! - **Transcripts** (`getSessionInfo`, `forkSession`, `getSessionMessages`, `deleteSession`): the
//!   real `@anthropic-ai/claude-agent-sdk` 0.3.301 file functions in the plugin sidecar.

pub mod accounts;
pub mod bridge;
pub mod convert;
pub mod hooks;
pub mod js;
pub mod probe;
pub mod process;
pub mod session;
pub mod sessions;
pub mod stream;
pub mod sync;
pub mod transcripts;
pub mod websearch;

use std::collections::{BTreeMap, HashMap};
use std::sync::{Arc, Weak};

use oc_ai::catalog::registry::{CatalogPlugin, ProviderEditor};
use oc_ai::catalog::types::{Activation, Capabilities, Limit, ModelInfo, ProviderInfo, Variant};
use oc_ai::credentials::{ConnectionStatus, Integrations, Method};
use parking_lot::{Mutex, RwLock};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::sync::watch;

use crate::bus::Bus;
use probe::ProbeResult;

pub const PLUGIN_ID: &str = "opencode.provider.claude-agent";
pub const PROVIDER_ID: &str = "claude-agent";
pub const PACKAGE: &str = "@opencode/claude-agent";
pub const METHOD_ID: &str = "claude-login";

/// `ClaudeStatus.State`.
#[derive(Debug, Clone, PartialEq)]
pub enum State {
    Checking,
    Missing,
    Failed(String),
    /// The probe, and the `CLAUDE_CONFIG_DIR` it ran with.
    Ready(ProbeResult, Option<String>),
}

/// One model of the saved catalog.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CatalogModel {
    pub id: String,
    pub name: String,
    pub efforts: Vec<String>,
    #[serde(rename = "contextWindow", default, skip_serializing_if = "Option::is_none", serialize_with = "js_number")]
    pub context_window: Option<f64>,
    #[serde(rename = "resolvedModel", default, skip_serializing_if = "Option::is_none")]
    pub resolved_model: Option<String>,
    /// A model ID that Claude Code no longer lists, or lists under another ID: still usable, kept out of pickers.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hidden: Option<bool>,
}

impl CatalogModel {
    fn is_hidden(&self) -> bool {
        self.hidden == Some(true)
    }
}

/// The model list kept between starts (plugin KV `models`), so opencode can list models without running `claude`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Catalog {
    #[serde(rename = "priceSource", default, skip_serializing_if = "Option::is_none")]
    pub price_source: Option<String>,
    pub models: Vec<CatalogModel>,
}

const CATALOG_KEY: &str = "models";

/// Numbers saved like JSON.stringify writes them (integral floats without a fraction).
fn js_number<S: serde::Serializer>(value: &Option<f64>, serializer: S) -> Result<S::Ok, S::Error> {
    match value {
        Some(v) => oc_ai::json::num(*v).serialize(serializer),
        None => serializer.serialize_none(),
    }
}

impl Catalog {
    /// `Schema.decodeUnknownOption(Catalog)`; a malformed value is no catalog.
    pub fn decode(value: Option<&Value>) -> Option<Catalog> {
        let object = value?.as_object()?;
        if object.get("priceSource").is_some_and(|v| !v.is_string()) {
            return None;
        }
        for model in object.get("models")?.as_array()? {
            let m = model.as_object()?;
            let strings = |k: &str| m.get(k).is_some_and(Value::is_string);
            if !strings("id") || !strings("name") || !m.get("efforts").and_then(Value::as_array).is_some_and(|e| e.iter().all(Value::is_string)) {
                return None;
            }
            if m.get("contextWindow").is_some_and(|v| !v.is_number())
                || m.get("resolvedModel").is_some_and(|v| !v.is_string())
                || m.get("hidden").is_some_and(|v| !v.is_boolean())
            {
                return None;
            }
        }
        serde_json::from_value(Value::Object(object.clone())).ok()
    }
}

/// `PRICE_SOURCES`: Mantle is Bedrock serving Anthropic's API, so it uses Anthropic's prices.
fn price_provider(api_provider: &str) -> Option<&'static str> {
    Some(match api_provider {
        "firstParty" | "anthropicAws" | "anthropicGoogleCloud" | "mantle" => "anthropic",
        "bedrock" => "amazon-bedrock",
        "vertex" => "google-vertex",
        "foundry" => "azure",
        _ => return None,
    })
}

/// A subscription covers usage, and a custom base URL or an enterprise gateway can bill anything, so they show no cost.
pub fn price_source(account: &Value, base_url: Option<&str>) -> Option<String> {
    if base_url.is_some_and(|u| !u.is_empty()) {
        return None;
    }
    let provider = account.get("apiProvider").and_then(Value::as_str)?;
    if provider == "firstParty" && account.get("apiKeySource").is_none() {
        return None;
    }
    price_provider(provider).map(String::from)
}

/// Claude Code lists a model under whatever name the chat was started with, and each account caches its own list,
/// while a chat or the config may hold any ID that was ever listed. So no ID is dropped, and a model already offered
/// under one ID keeps it when Claude Code lists it under another (2.4.1).
/// Context windows only come from the full check, so a chat's startup report keeps the saved ones.
pub fn from_claude(models: &[Value], saved: &[CatalogModel]) -> Vec<CatalogModel> {
    let value = |model: &Value| model.get("value").and_then(Value::as_str).unwrap_or_default().to_string();
    // "default" follows Claude Code's own default, which can change under a running opencode session
    let listed: Vec<&Value> = models.iter().filter(|model| value(model) != "default").collect();
    let mut result: indexmap::IndexMap<String, CatalogModel> = indexmap::IndexMap::new();
    for model in &listed {
        let id = value(model);
        let resolved = model.get("resolvedModel").and_then(Value::as_str).map(String::from);
        let own = saved.iter().find(|item| item.id == id);
        let offered = if own.is_some_and(|own| !own.is_hidden()) {
            None
        } else {
            saved.iter().find(|item| {
                !item.is_hidden()
                    && item.resolved_model.is_some()
                    && item.resolved_model == resolved
                    // An alias that is still listed has moved to another version, and this row is the old one
                    && !listed.iter().any(|other| value(other) == item.id)
            })
        };
        let entry = CatalogModel {
            id: id.clone(),
            name: model.get("displayName").and_then(Value::as_str).unwrap_or_default().to_string(),
            efforts: model
                .get("supportedEffortLevels")
                .and_then(Value::as_array)
                .map(|e| e.iter().filter_map(Value::as_str).map(String::from).collect())
                .unwrap_or_default(),
            context_window: model.get("contextWindow").and_then(Value::as_f64).or_else(|| offered.or(own).and_then(|m| m.context_window)),
            resolved_model: resolved,
            hidden: None,
        };
        match offered {
            Some(offered) => {
                result.insert(offered.id.clone(), CatalogModel { id: offered.id.clone(), ..entry.clone() });
                result.insert(id, CatalogModel { hidden: Some(true), ..entry });
            }
            None => {
                result.insert(id, entry);
            }
        }
    }
    for model in saved {
        if !result.contains_key(&model.id) {
            result.insert(model.id.clone(), CatalogModel { hidden: Some(true), ..model.clone() });
        }
    }
    result.into_values().collect()
}

/// `toModel(model, prices)`.
pub fn to_model(model: &CatalogModel, prices: Option<&indexmap::IndexMap<String, ModelInfo>>) -> ModelInfo {
    let mut info = ModelInfo::default_for(PROVIDER_ID, &model.id);
    // The 1M context version's price tiers are on the base model
    let id = model.resolved_model.as_deref().unwrap_or(&model.id);
    let id = id.strip_suffix("[1m]").unwrap_or(id);
    // Mantle names models `anthropic.claude-…`, and Vertex lists current models as `claude-…@default`
    let priced =
        prices.and_then(|p| p.get(id).or_else(|| p.get(id.strip_prefix("anthropic.").unwrap_or(id))).or_else(|| p.get(&format!("{id}@default"))));
    info.name = model.name.clone();
    // The SDK reports context windows before a prompt, but not output limits. 0 means unknown in OpenCode.
    info.limit = Limit { context: model.context_window.map(|c| c as i64).unwrap_or(Limit::default().context), input: None, output: 0 };
    // OpenCode picks the title model by family (Model.small).
    info.family = model.id.contains("haiku").then(|| "claude-haiku".to_string());
    info.capabilities = Capabilities {
        tools: true,
        input: vec!["text".into(), "image".into(), "pdf".into()],
        output: vec!["text".into()],
        ..Capabilities::default()
    };
    info.variants = model
        .efforts
        .iter()
        .map(|effort| Variant {
            id: effort.clone(),
            settings: Some(json!({ "effort": effort }).as_object().cloned().unwrap_or_default()),
            headers: None,
            body: None,
        })
        .collect();
    info.cost = priced.map(|m| m.cost.clone()).unwrap_or_default();
    // opencode keeps a deprecated model out of its pickers and still runs chats on it
    if model.is_hidden() {
        info.status = oc_ai::catalog::types::ModelStatus::Deprecated;
    }
    info
}

/// The provider transform (`ClaudeProvider.register`), run among the provider plugins.
pub struct ProviderPlugin {
    pub catalog: Catalog,
}

impl CatalogPlugin for ProviderPlugin {
    fn providers(&mut self, editor: &mut ProviderEditor) {
        let prices = self.catalog.price_source.as_deref().and_then(|s| editor.get(s)).map(|r| r.models.clone());
        let models = self.catalog.models.iter().map(|m| to_model(m, prices.as_ref())).collect();
        let mut info = ProviderInfo::empty(PROVIDER_ID);
        info.name = "Claude Agent".into();
        info.activation = Activation::Auto;
        info.package = format!("aisdk:{PACKAGE}");
        editor.add(info, models, None);
    }
}

/// `ClaudeQueryOptions.connectionEnv`: the provider's settings, headers and body as Claude Code environment.
pub fn connection_env(settings: Option<&serde_json::Map<String, Value>>, headers: Option<&Value>, body: Option<&Value>) -> Vec<(String, String)> {
    let mut env = vec![];
    let get = |k: &str| settings.and_then(|s| s.get(k));
    for (key, name) in [("baseURL", "ANTHROPIC_BASE_URL"), ("apiKey", "ANTHROPIC_API_KEY"), ("authToken", "ANTHROPIC_AUTH_TOKEN")] {
        if let Some(Value::String(v)) = get(key) {
            env.push((name.to_string(), v.clone()));
        }
    }
    for (key, name) in [("headerTimeout", "API_TIMEOUT_MS"), ("chunkTimeout", "CLAUDE_STREAM_IDLE_TIMEOUT_MS")] {
        if let Some(n) = get(key).and_then(Value::as_f64) {
            env.push((name.to_string(), js::number(n)));
        }
    }
    // The account rotation picks the folder for each request. The login check and model list use the first.
    if let Some(first) = config_dirs(get("configDir")).into_iter().next() {
        env.push(("CLAUDE_CONFIG_DIR".into(), first));
    }
    let lines: Vec<String> = headers
        .and_then(Value::as_object)
        .map(|h| h.iter().map(|(k, v)| format!("{k}: {}", v.as_str().map(String::from).unwrap_or_else(|| oc_ai::json::js_string(v)))).collect())
        .unwrap_or_default();
    if !lines.is_empty() {
        env.push(("ANTHROPIC_CUSTOM_HEADERS".into(), lines.join("\n")));
    }
    if let Some(body) = body.and_then(Value::as_object).filter(|b| !b.is_empty()) {
        env.push(("CLAUDE_CODE_EXTRA_BODY".into(), js::stringify(&Value::Object(body.clone()))));
    }
    env
}

/// `ClaudeQueryOptions.configDirs(value)`: the `configDir` setting (one folder or a list) as absolute folders.
pub fn config_dirs(value: Option<&Value>) -> Vec<String> {
    let items: Vec<&Value> = match value {
        Some(Value::Array(items)) => items.iter().collect(),
        Some(value) => vec![value],
        None => vec![],
    };
    items
        .into_iter()
        .filter_map(Value::as_str)
        .map(|item| {
            // Claude Code does not expand ~, so a configDir like ~/.claude-work would point inside the project.
            // An absolute path also means Claude Code and our own transcript lookups agree on the folder.
            let dir = match item.strip_prefix("~/") {
                Some(rest) => oc_util::global::os_home().join(rest).to_string_lossy().into_owned(),
                None => item.to_string(),
            };
            resolve_path(&dir)
        })
        .collect()
}

/// Node's `path.resolve(p)`: absolute against the working directory, with `.` and `..` resolved.
pub fn resolve_path(p: &str) -> String {
    let cwd = std::env::current_dir().unwrap_or_default();
    #[cfg(windows)]
    {
        let joined = cwd.join(p);
        std::path::absolute(&joined).unwrap_or(joined).to_string_lossy().into_owned()
    }
    #[cfg(not(windows))]
    {
        use std::path::{Component, PathBuf};
        let mut out = PathBuf::from("/");
        for component in cwd.join(p).components() {
            match component {
                Component::ParentDir => {
                    out.pop();
                }
                Component::Normal(part) => out.push(part),
                Component::RootDir | Component::CurDir | Component::Prefix(_) => {}
            }
        }
        out.to_string_lossy().into_owned()
    }
}

/// `ClaudeQueryOptions.withoutDefaultFolder`: Claude Code keeps its state in ~/.claude.json only while
/// `CLAUDE_CONFIG_DIR` is unset. Naming the default folder moves that to ~/.claude/.claude.json, which can hold an
/// older model list, so the default folder is used by not naming it.
pub fn without_default_folder(connection: &[(String, String)], process: &BTreeMap<String, String>) -> Vec<(String, String)> {
    let default = oc_util::global::os_home().join(".claude").to_string_lossy().into_owned();
    let named = connection.iter().any(|(k, v)| k == "CLAUDE_CONFIG_DIR" && *v == default);
    // With the variable already set for opencode, leaving it out would run that folder instead.
    if !named || process.contains_key("CLAUDE_CONFIG_DIR") {
        return connection.to_vec();
    }
    connection.iter().filter(|(k, _)| k != "CLAUDE_CONFIG_DIR").cloned().collect()
}

/// The `CLAUDE_CONFIG_DIR` of a connection.
pub fn config_dir_of(connection: &[(String, String)]) -> Option<String> {
    connection.iter().find(|(k, _)| k == "CLAUDE_CONFIG_DIR").map(|(_, v)| v.clone())
}

/// `ClaudeProbe.loginMessage(configDir)`.
pub fn login_message(config_dir: Option<&str>) -> String {
    let Some(dir) = config_dir else { return "Claude Code is not logged in. Run `claude auth login` in a terminal.".into() };
    if cfg!(windows) {
        return format!("Claude Code is not logged in. Run `$env:CLAUDE_CONFIG_DIR = \"{dir}\"; claude auth login` in PowerShell.");
    }
    format!("Claude Code is not logged in. Run `CLAUDE_CONFIG_DIR=\"{dir}\" claude auth login` in a terminal.")
}

/// `problem(state)`: the connection status a state shows.
pub fn problem(state: &State) -> Option<ConnectionStatus> {
    match state {
        State::Missing => Some(ConnectionStatus::needs_auth(
            "Claude Code is not installed. Install it, then run `claude auth login`.",
            Some("https://code.claude.com/docs/en/setup".into()),
        )),
        // The message already says what went wrong with Claude Code.
        State::Failed(message) => Some(ConnectionStatus::needs_auth(message.clone(), None)),
        State::Ready(probe, config_dir) if !probe::is_logged_in(&probe.account) => {
            Some(ConnectionStatus::needs_auth(login_message(config_dir.as_deref()), None))
        }
        _ => None,
    }
}

/// The integration transform (`ClaudeIntegration.register`).
pub fn register_integration(integrations: &Integrations) {
    integrations.update(PROVIDER_ID, |integration| integration.name = "Claude Agent".into());
    integrations
        .register_method(PROVIDER_ID, Method::External { id: METHOD_ID.into(), label: "Claude Code login (claude auth login)".into(), form: None });
}

/// The plugin's KV (`ctx.storage`, namespace of `opencode.provider.claude-agent`).
pub struct Kv {
    db: oc_storage::Database,
    namespace: String,
}

impl Kv {
    pub fn new(db: oc_storage::Database) -> Kv {
        let hex: String = PLUGIN_ID.encode_utf16().map(|u| format!("{u:04x}")).collect();
        Kv { db, namespace: format!("plugin:{hex}:") }
    }
}

#[async_trait::async_trait]
impl sessions::Storage for Kv {
    async fn get(&self, key: &str) -> Option<Value> {
        self.db.kv_get(&format!("{}{key}", self.namespace)).ok().flatten()
    }
    async fn set(&self, key: &str, value: Value) {
        if let Err(error) = self.db.kv_set(&format!("{}{key}", self.namespace), &value) {
            tracing::warn!(%error, key, "failed to save Claude Agent state");
        }
    }
    async fn remove(&self, key: &str) {
        if let Err(error) = self.db.kv_remove(&format!("{}{key}", self.namespace)) {
            tracing::warn!(%error, key, "failed to remove Claude Agent state");
        }
    }
    async fn scan(&self, prefix: &str) -> Vec<String> {
        let mut keys = vec![];
        let mut after: Option<String> = None;
        loop {
            let Ok((entries, next)) = self.db.kv_scan(&format!("{}{prefix}", self.namespace), after.as_deref(), Some(1000)) else { break };
            keys.extend(entries.into_iter().filter_map(|(key, _)| key.strip_prefix(&self.namespace).map(String::from)));
            match next {
                Some(next) => after = Some(next),
                None => break,
            }
        }
        keys
    }
}

/// One location's plugin instance.
pub struct Location {
    key: String,
    integrations: Arc<Integrations>,
    state: watch::Sender<State>,
    catalog: RwLock<Catalog>,
    refreshing: tokio::sync::Mutex<()>,
    /// Models a chat reported without a context size that a full check already ran for (`checked`).
    checked: parking_lot::Mutex<std::collections::HashSet<String>>,
    /// Whether the integration has an active connection, which offers the web search provider (2.4.1).
    connected: std::sync::atomic::AtomicBool,
    owner: Weak<ClaudeAgent>,
}

impl sessions::Reporter for Location {
    fn missing(&self) {
        self.report(State::Missing);
    }
    /// A chat's own process reported (`fromChat`). A model with its own `configDir` runs another account, which must
    /// not replace the provider's login, prices and models. Every folder in the provider's list is its own, so those
    /// follow the folder a chat last started on (2.4.1).
    fn ready(&self, probe: ProbeResult, config_dir: Option<String>) {
        if let Some(owner) = self.owner.upgrade() {
            let process = process_env();
            let mut own = vec![config_dir_of(&without_default_folder(&owner.connection(&self.key), &process))];
            for dir in owner.config_dirs(&self.key) {
                own.push(config_dir_of(&without_default_folder(&[("CLAUDE_CONFIG_DIR".into(), dir)], &process)));
            }
            if !own.contains(&config_dir) {
                return;
            }
        }
        self.report_from(State::Ready(probe, config_dir), true);
    }
}

impl Location {
    /// `status.report(next)` (and the state's subscribers: the connection status and the catalog).
    pub fn report(&self, next: State) {
        self.report_from(next, false);
    }

    /// [`Location::report`]; `from_chat` when a chat's own process found `next` as it started.
    fn report_from(&self, next: State, from_chat: bool) {
        self.state.send_replace(next.clone());
        let Some(owner) = self.owner.upgrade() else { return };
        let me = owner.location(&self.key);
        tokio::spawn(async move {
            let Some(me) = me else { return };
            me.changed(&owner, next, from_chat).await;
        });
    }

    async fn changed(self: &Arc<Self>, owner: &Arc<ClaudeAgent>, state: State, from_chat: bool) {
        if state == State::Checking {
            return;
        }
        // The connection's problem, if any.
        if let Ok(Some(connection)) = self.integrations.active(PROVIDER_ID).await {
            if self.integrations.set_status(PROVIDER_ID, &connection, problem(&state))
                && let Some(plugins) = owner.plugins.upgrade()
            {
                plugins.integrations_changed(&self.key);
            }
        }
        let State::Ready(probe, _) = state else { return };
        let base_url = owner
            .connection(&self.key)
            .into_iter()
            .find(|(k, _)| k == "ANTHROPIC_BASE_URL")
            .map(|(_, v)| v)
            .or_else(|| std::env::var("ANTHROPIC_BASE_URL").ok());
        // Another project folder may have saved IDs since this one loaded
        let stored = Catalog::decode(sessions::Storage::get(&Kv::new(owner.db.clone()), CATALOG_KEY).await.as_ref());
        let next = {
            let current = self.catalog.read();
            let saved = stored.as_ref().unwrap_or(&current);
            Catalog { price_source: price_source(&probe.account, base_url.as_deref()), models: from_claude(&probe.models, &saved.models) }
        };
        // A freshly started process reports its models without context sizes, which only the full check finds: run it
        // once per such model, so the model doesn't fall back to the default window.
        let unknown: Vec<String> = {
            let checked = self.checked.lock();
            next.models.iter().filter(|m| !m.is_hidden() && m.context_window.is_none() && !checked.contains(&m.id)).map(|m| m.id.clone()).collect()
        };
        if from_chat && !unknown.is_empty() {
            self.checked.lock().extend(unknown);
            let (me, owner) = (self.clone(), owner.clone());
            tokio::spawn(async move { me.refresh(&owner).await });
        }
        if serde_json::to_string(&next).ok() == serde_json::to_string(&*self.catalog.read()).ok() {
            return;
        }
        *self.catalog.write() = next.clone();
        sessions::Storage::set(&Kv::new(owner.db.clone()), CATALOG_KEY, serde_json::to_value(&next).unwrap_or(Value::Null)).await;
        owner.provider_reload(&self.key);
    }

    /// `status.refresh()`: serialized, so a reconnect checks again after startup rather than racing it.
    pub async fn refresh(&self, owner: &Arc<ClaudeAgent>) {
        let _permit = self.refreshing.lock().await;
        let io = transcripts::RealIo::new(owner.plugins.clone());
        let Some(executable) = sessions::Io::which(&io) else {
            self.report(State::Missing);
            return;
        };
        let connection = without_default_folder(&owner.connection(&self.key), &sessions::Io::env(&io));
        let config_dir = config_dir_of(&connection);
        let env = sessions::base_env(sessions::Io::env(&io), &connection);
        let bun = if process::is_script(&executable) { sessions::Io::bun(&io).await } else { None };
        let options = process::Options {
            executable,
            bun,
            cwd: None,
            env,
            model: None,
            effort: None,
            resume: None,
            session_id: None,
            include_partial_messages: false,
            persist: false,
            system: process::SystemPrompt::Default,
            hooks: None,
            tools: None,
            web_search: false,
        };
        let next = match probe::run(options).await {
            Ok(probe) => State::Ready(probe, config_dir),
            Err(message) => {
                tracing::warn!(cause = %message, "Claude Code login check failed");
                State::Failed(message)
            }
        };
        self.report(next);
    }

    pub fn catalog(&self) -> Catalog {
        self.catalog.read().clone()
    }

    pub fn state(&self) -> State {
        self.state.borrow().clone()
    }
}

/// The plugin across locations, with the process-wide session registry.
pub struct ClaudeAgent {
    db: oc_storage::Database,
    bus: Bus,
    plugins: Weak<crate::plugins::Plugins>,
    pub registry: Arc<sessions::Registry>,
    locations: Mutex<HashMap<String, Arc<Location>>>,
    subscribed: std::sync::Once,
    me: Weak<ClaudeAgent>,
}

impl ClaudeAgent {
    pub fn new(db: oc_storage::Database, bus: Bus, plugins: Weak<crate::plugins::Plugins>) -> Arc<ClaudeAgent> {
        Arc::new_cyclic(|me| ClaudeAgent {
            db,
            bus,
            plugins,
            registry: sessions::Registry::new(),
            locations: Mutex::new(HashMap::new()),
            subscribed: std::sync::Once::new(),
            me: me.clone(),
        })
    }

    pub fn location(&self, key: &str) -> Option<Arc<Location>> {
        self.locations.lock().get(key).cloned()
    }

    /// The plugin starts at a location (its first catalog build): the integration is registered, the
    /// saved catalog loaded, and without a saved list but with a connection the login check runs.
    pub async fn activate(&self, key: &str, integrations: &Arc<Integrations>) -> Arc<Location> {
        register_integration(integrations);
        if let Some(location) = self.location(key) {
            return location;
        }
        let saved = Catalog::decode(sessions::Storage::get(&Kv::new(self.db.clone()), CATALOG_KEY).await.as_ref()).unwrap_or_default();
        let empty = saved.models.is_empty();
        let location = {
            let mut locations = self.locations.lock();
            if let Some(existing) = locations.get(key) {
                return existing.clone();
            }
            let location = Arc::new(Location {
                key: key.to_string(),
                integrations: integrations.clone(),
                state: watch::channel(State::Checking).0,
                catalog: RwLock::new(saved),
                refreshing: tokio::sync::Mutex::new(()),
                checked: Default::default(),
                connected: Default::default(),
                owner: self.me.clone(),
            });
            locations.insert(key.to_string(), location.clone());
            location
        };
        self.registry.attach();
        if let Ok(runtime) = tokio::runtime::Handle::try_current() {
            self.subscribe(&runtime);
        }
        let connected = matches!(integrations.active(PROVIDER_ID).await, Ok(Some(_)));
        if connected {
            location.connected.store(true, std::sync::atomic::Ordering::SeqCst);
            self.websearch_reload(key);
        }
        // Without a saved list there is no model to pick, so the first message that would fill it can never be sent.
        if empty && connected {
            let (location, owner) = (location.clone(), self.me.upgrade());
            tokio::spawn(async move {
                if let Some(owner) = owner {
                    location.refresh(&owner).await;
                }
            });
        }
        location
    }

    /// The location closed (its plugin scope ended).
    pub fn close(&self, key: &str) {
        if self.locations.lock().remove(key).is_some() {
            self.registry.detach();
        }
    }

    /// The catalog transform for location `key` (`None` before the plugin started there).
    pub fn provider_plugin(&self, key: &str) -> Option<ProviderPlugin> {
        self.location(key).map(|l| ProviderPlugin { catalog: l.catalog() })
    }

    /// Part of the catalog cache key: the saved catalog the transform would add.
    pub fn fingerprint(&self, key: &str) -> String {
        self.location(key).map(|l| serde_json::to_string(&l.catalog()).unwrap_or_default()).unwrap_or_default()
    }

    /// `ClaudeProvider.connection(ctx)`: the provider's settings, headers and body as environment.
    pub fn connection(&self, key: &str) -> Vec<(String, String)> {
        let Some(plugins) = self.plugins.upgrade() else { return vec![] };
        let Some(catalog) = plugins.catalog(key) else { return vec![] };
        let Some(provider) = catalog.provider(PROVIDER_ID) else { return vec![] };
        let headers = provider.headers.as_ref().map(|h| serde_json::to_value(h).unwrap_or(Value::Null));
        let body = provider.body.as_ref().map(|b| Value::Object(b.clone()));
        connection_env(provider.settings.as_ref(), headers.as_ref(), body.as_ref())
    }

    /// `ClaudeProvider.configDirs(ctx)`: the provider's `configDir` setting as folders.
    pub fn config_dirs(&self, key: &str) -> Vec<String> {
        let Some(plugins) = self.plugins.upgrade() else { return vec![] };
        let Some(catalog) = plugins.catalog(key) else { return vec![] };
        let Some(provider) = catalog.provider(PROVIDER_ID) else { return vec![] };
        config_dirs(provider.settings.as_ref().and_then(|s| s.get("configDir")))
    }

    /// The web search provider of location `key`, offered while the integration is connected. It is never made the
    /// default: users pick it in their config.
    pub fn websearch_provider(&self, key: &str) -> Option<Arc<dyn oc_tools::websearch::SearchProvider>> {
        let location = self.location(key)?;
        if !location.connected.load(std::sync::atomic::Ordering::SeqCst) {
            return None;
        }
        Some(Arc::new(websearch::ClaudeSearch { owner: self.me.clone(), key: key.to_string() }))
    }

    /// `ctx.websearch.reload()`.
    fn websearch_reload(&self, key: &str) {
        let (plugins, key) = (self.plugins.clone(), key.to_string());
        tokio::spawn(async move {
            if let Some(plugins) = plugins.upgrade() {
                plugins.websearch_reload(&key).await;
            }
        });
    }

    fn provider_reload(&self, key: &str) {
        if let Some(plugins) = self.plugins.upgrade() {
            plugins.provider_reload(key);
        }
    }

    /// `credential.switched` for `claude-agent` checks again at every location; a session that stopped
    /// running starts its parked turn's idle timer; a deleted session's process stops and its records go.
    fn subscribe(&self, runtime: &tokio::runtime::Handle) {
        let me = self.me.clone();
        let bus = self.bus.clone();
        self.subscribed.call_once(|| {
            let mut events = bus.subscribe();
            runtime.spawn(async move {
                loop {
                    let event = match events.recv().await {
                        Ok(event) => event,
                        Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                        Err(_) => return,
                    };
                    let Some(owner) = me.upgrade() else { return };
                    match event.kind.as_str() {
                        "credential.switched" if event.data.get("integrationID").and_then(Value::as_str) == Some(PROVIDER_ID) => {
                            let locations: Vec<Arc<Location>> = owner.locations.lock().values().cloned().collect();
                            for location in locations {
                                let owner = owner.clone();
                                tokio::spawn(async move {
                                    let connected = matches!(location.integrations.active(PROVIDER_ID).await, Ok(Some(_)));
                                    location.connected.store(connected, std::sync::atomic::Ordering::SeqCst);
                                    owner.websearch_reload(&location.key);
                                    location.refresh(&owner).await
                                });
                            }
                        }
                        "session.execution.succeeded" | "session.execution.failed" | "session.execution.interrupted" => {
                            if let Some(id) = event.data.get("sessionID").and_then(Value::as_str) {
                                owner.registry.idle(id);
                            }
                        }
                        // OpenCode deleted the session, so nothing can resume it (2.4.1).
                        "session.deleted" => {
                            if let Some(id) = event.data.get("sessionID").and_then(Value::as_str).map(String::from) {
                                let owner = owner.clone();
                                tokio::spawn(async move {
                                    let storage = Kv::new(owner.db.clone());
                                    owner.registry.remove(&storage, &id).await;
                                    sessions::Storage::remove(&storage, &accounts::key(&id)).await;
                                });
                            }
                        }
                        _ => {}
                    }
                }
            });
        });
    }

    /// The request context of location `key`.
    pub fn context(&self, key: &str) -> Option<sessions::Context> {
        let location = self.location(key)?;
        Some(sessions::Context {
            io: Arc::new(transcripts::RealIo::new(self.plugins.clone())),
            storage: Arc::new(Kv::new(self.db.clone())),
            reporter: location,
            directory: key.to_string(),
        })
    }
}

/// The process environment (`process.env`), live.
pub fn process_env() -> BTreeMap<String, String> {
    std::env::vars().collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prices_follow_the_api_provider() {
        assert_eq!(price_source(&json!({ "apiProvider": "firstParty" }), None), None);
        assert_eq!(price_source(&json!({ "apiProvider": "firstParty", "apiKeySource": "ANTHROPIC_API_KEY" }), None).as_deref(), Some("anthropic"));
        assert_eq!(price_source(&json!({ "apiProvider": "bedrock" }), None).as_deref(), Some("amazon-bedrock"));
        assert_eq!(price_source(&json!({ "apiProvider": "vertex" }), None).as_deref(), Some("google-vertex"));
        assert_eq!(price_source(&json!({ "apiProvider": "foundry" }), None).as_deref(), Some("azure"));
        assert_eq!(price_source(&json!({ "apiProvider": "bedrock" }), Some("http://gateway")), None);
        assert_eq!(price_source(&json!({}), None), None);
    }

    #[test]
    fn catalog_keeps_saved_context_windows() {
        let saved =
            vec![CatalogModel { id: "opus".into(), name: "Opus".into(), efforts: vec![], context_window: Some(1_000_000.0), resolved_model: None, hidden: None }];
        let models = from_claude(
            &[
                json!({ "value": "default", "displayName": "Default" }),
                json!({ "value": "opus", "displayName": "Opus 5.5", "resolvedModel": "claude-opus-5-5", "supportedEffortLevels": ["low", "max"] }),
                json!({ "value": "haiku", "displayName": "Haiku 5.5", "contextWindow": 200000 }),
            ],
            &saved,
        );
        assert_eq!(
            serde_json::to_value(&models).unwrap(),
            json!([
                { "id": "opus", "name": "Opus 5.5", "efforts": ["low", "max"], "contextWindow": 1000000, "resolvedModel": "claude-opus-5-5" },
                { "id": "haiku", "name": "Haiku 5.5", "efforts": [], "contextWindow": 200000 },
            ])
        );
        let model = to_model(&models[1], None);
        assert_eq!(model.family.as_deref(), Some("claude-haiku"));
        assert_eq!(model.limit.output, 0);
        assert_eq!(to_model(&models[0], None).variants.len(), 2);
    }

    fn saved(id: &str, name: &str, resolved: Option<&str>) -> CatalogModel {
        CatalogModel { id: id.into(), name: name.into(), efforts: vec![], context_window: None, resolved_model: resolved.map(String::from), hidden: None }
    }

    /// The models of upstream's `startup.jsonl` recording that the 2.4.1 tests rely on.
    fn listed() -> Vec<Value> {
        vec![
            json!({ "value": "default", "resolvedModel": "claude-opus-5-5", "displayName": "Default (recommended)" }),
            json!({ "value": "opus", "resolvedModel": "claude-opus-5-5", "displayName": "Opus 5.5" }),
            json!({ "value": "fable", "resolvedModel": "claude-fable-5-1", "displayName": "Fable 5.1" }),
            json!({ "value": "sonnet", "resolvedModel": "claude-sonnet-5-5", "displayName": "Sonnet 5.5" }),
            json!({ "value": "haiku", "resolvedModel": "claude-haiku-5-5", "displayName": "Haiku 5.5" }),
            json!({ "value": "claude-sonnet-5", "resolvedModel": "claude-sonnet-5", "displayName": "Sonnet 5" }),
        ]
    }

    // plugin.test.ts "keeps every saved model ID usable when Claude Code lists a model under another name" (2.4.1)
    #[test]
    fn renamed_models_keep_every_saved_id() {
        let saved = vec![
            // The recorded list calls this model `fable`
            CatalogModel { context_window: Some(500_000.0), ..saved("claude-fable-5-1", "Fable 5.1", Some("claude-fable-5-1")) },
            // and lists Sonnet 5 under its own ID, next to a `sonnet` that has moved to a newer version
            saved("sonnet", "Sonnet 5", Some("claude-sonnet-5")),
            saved("claude-retired", "Retired", None),
        ];
        let models = from_claude(&listed(), &saved);
        let status = |id: &str| {
            let model = models.iter().find(|m| m.id == id).unwrap_or_else(|| panic!("{id} is listed"));
            to_model(model, None).status
        };
        use oc_ai::catalog::types::ModelStatus;
        assert_eq!(status("claude-fable-5-1"), ModelStatus::Active);
        assert_eq!(status("fable"), ModelStatus::Deprecated);
        assert_eq!(status("claude-sonnet-5"), ModelStatus::Active);
        assert_eq!(status("sonnet"), ModelStatus::Active);
        assert_eq!(status("claude-retired"), ModelStatus::Deprecated);
        let ids: Vec<&str> = models.iter().map(|m| m.id.as_str()).collect();
        assert_eq!(ids, ["opus", "claude-fable-5-1", "fable", "sonnet", "haiku", "claude-sonnet-5", "claude-retired"]);
        // The kept ID takes the listed model's details and its own saved context window.
        assert_eq!(
            serde_json::to_value(&models[1..3]).unwrap(),
            json!([
                { "id": "claude-fable-5-1", "name": "Fable 5.1", "efforts": [], "contextWindow": 500000, "resolvedModel": "claude-fable-5-1" },
                { "id": "fable", "name": "Fable 5.1", "efforts": [], "contextWindow": 500000, "resolvedModel": "claude-fable-5-1", "hidden": true },
            ])
        );
        assert_eq!(serde_json::to_value(&models[6]).unwrap(), json!({ "id": "claude-retired", "name": "Retired", "efforts": [], "hidden": true }));
        // A hidden saved row is never the one a renamed model keeps.
        let again = from_claude(&listed(), &models);
        assert_eq!(serde_json::to_value(&again).unwrap(), serde_json::to_value(&models).unwrap());
        assert!(Catalog::decode(Some(&json!({ "models": [{ "id": "a", "name": "A", "efforts": [], "hidden": "yes" }] }))).is_none());
    }

    // accounts.test.ts "configDir takes one folder or a list, expanding ~, and the login check uses the first" (2.4.1)
    #[test]
    fn config_folders() {
        let home = oc_util::global::os_home().join(".claude-1").to_string_lossy().into_owned();
        assert_eq!(config_dirs(Some(&json!("~/.claude-1"))), vec![home.clone()]);
        assert_eq!(config_dirs(Some(&json!(["~/.claude-1", "/tmp/claude-2", 3]))), vec![home.clone(), resolve_path("/tmp/claude-2")]);
        assert_eq!(config_dirs(None), Vec::<String>::new());
        let settings = json!({ "configDir": ["~/.claude-1", "/tmp/claude-2"] });
        assert_eq!(connection_env(settings.as_object(), None, None), vec![("CLAUDE_CONFIG_DIR".to_string(), home)]);
        // Relative folders resolve against the working directory, and `.` / `..` go.
        let cwd = std::env::current_dir().unwrap();
        assert_eq!(resolve_path("a/./b/../c"), cwd.join("a").join("c").to_string_lossy());
        #[cfg(unix)]
        assert_eq!(resolve_path("/x//y/../z/"), "/x/z");
    }

    // plugin.test.ts "runs the default folder without naming it, so Claude Code keeps its usual state file" (2.4.1)
    #[test]
    fn the_default_folder_is_not_named() {
        let default = oc_util::global::os_home().join(".claude").to_string_lossy().into_owned();
        let connection = vec![("ANTHROPIC_BASE_URL".to_string(), "http://x".to_string()), ("CLAUDE_CONFIG_DIR".to_string(), default.clone())];
        let none = BTreeMap::new();
        assert_eq!(without_default_folder(&connection, &none), connection[..1].to_vec());
        // With the variable already set for opencode, leaving it out would run that folder instead.
        let set = BTreeMap::from([("CLAUDE_CONFIG_DIR".to_string(), "/elsewhere".to_string())]);
        assert_eq!(without_default_folder(&connection, &set), connection);
        let other = vec![("CLAUDE_CONFIG_DIR".to_string(), format!("{default}-work"))];
        assert_eq!(without_default_folder(&other, &none), other);
    }

    #[test]
    fn connection_environment() {
        let settings = json!({ "baseURL": "http://x", "apiKey": "k", "headerTimeout": 5000, "other": 1 });
        let env = connection_env(settings.as_object(), Some(&json!({ "A": "1", "B": "2" })), Some(&json!({ "x": 1 })));
        assert_eq!(
            env,
            vec![
                ("ANTHROPIC_BASE_URL".to_string(), "http://x".to_string()),
                ("ANTHROPIC_API_KEY".into(), "k".into()),
                ("API_TIMEOUT_MS".into(), "5000".into()),
                ("ANTHROPIC_CUSTOM_HEADERS".into(), "A: 1\nB: 2".into()),
                ("CLAUDE_CODE_EXTRA_BODY".into(), "{\"x\":1}".into()),
            ]
        );
    }

    #[test]
    fn problems() {
        assert_eq!(problem(&State::Missing).unwrap().url.as_deref(), Some("https://code.claude.com/docs/en/setup"));
        assert_eq!(problem(&State::Failed("boom".into())).unwrap().message, "boom");
        let logged_out = ProbeResult { version: None, account: json!({ "tokenSource": "none" }), models: vec![] };
        assert_eq!(
            problem(&State::Ready(logged_out.clone(), None)).unwrap().message,
            "Claude Code is not logged in. Run `claude auth login` in a terminal."
        );
        let folder = resolve_path("/claude-work");
        let expected = if cfg!(windows) {
            format!("Claude Code is not logged in. Run `$env:CLAUDE_CONFIG_DIR = \"{folder}\"; claude auth login` in PowerShell.")
        } else {
            format!("Claude Code is not logged in. Run `CLAUDE_CONFIG_DIR=\"{folder}\" claude auth login` in a terminal.")
        };
        assert_eq!(problem(&State::Ready(logged_out, Some(folder))).unwrap().message, expected);
        let logged_in = ProbeResult { version: None, account: json!({ "email": "a" }), models: vec![] };
        assert!(problem(&State::Ready(logged_in, None)).is_none());
    }
}
