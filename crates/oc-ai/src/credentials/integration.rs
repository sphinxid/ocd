//! Integrations: methods, connections, credential resolution and connect attempts
//! (`packages/schema/src/integration.ts`, `packages/core/src/integration.ts`, `integration/connection.ts`).

use super::form::{self, FormAnswer};
use super::store::{CreateCredential, CredentialStore, CredentialUpdate, unique_label};
use super::{Clock, Credential, CredentialValue, EnvSource, Metadata, OAuthValue};
use async_trait::async_trait;
use indexmap::IndexMap;
use parking_lot::{Mutex, RwLock};
use serde::{Deserialize, Serialize, Serializer};
use serde_json::{Map, Value, json};
use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Weak};
use tokio::sync::broadcast;
use tokio_util::sync::CancellationToken;

/// Pending attempt lifetime (`attemptLifetime`).
pub const ATTEMPT_LIFETIME_MS: u64 = 10 * 60_000;
/// Terminal attempts are kept this long (`terminalRetention`).
pub const TERMINAL_RETENTION_MS: u64 = 60_000;
/// Scrubber period (`scrubInterval`).
pub const SCRUB_INTERVAL_MS: u64 = 30_000;
/// OAuth credentials are refreshed on resolve when they expire within this window.
pub const REFRESH_WINDOW_MS: u64 = 5 * 60_000;

pub type BoxFuture<T> = Pin<Box<dyn Future<Output = T> + Send + 'static>>;

/// Error raised by provider flows (an `Error` with a message).
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{message}")]
pub struct FlowError {
    pub message: String,
}

impl FlowError {
    pub fn new(message: impl Into<String>) -> Self {
        Self { message: message.into() }
    }
}

pub type FlowResult<T> = Result<T, FlowError>;

// ---------------------------------------------------------------------------------------------
// Methods
// ---------------------------------------------------------------------------------------------

/// `Integration.Method` (tagged by `type`). Forms are raw `Form.Field` JSON.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum Method {
    Oauth {
        id: String,
        label: String,
        #[serde(default)]
        form: Option<Vec<Value>>,
    },
    Command {
        id: String,
        label: String,
        command: Vec<String>,
    },
    Key {
        #[serde(default)]
        label: Option<String>,
        #[serde(default)]
        form: Option<Vec<Value>>,
    },
    External {
        id: String,
        label: String,
        #[serde(default)]
        form: Option<Vec<Value>>,
    },
    Env {
        names: Vec<String>,
    },
}

impl Method {
    pub fn kind(&self) -> &'static str {
        match self {
            Self::Oauth { .. } => "oauth",
            Self::Command { .. } => "command",
            Self::Key { .. } => "key",
            Self::External { .. } => "external",
            Self::Env { .. } => "env",
        }
    }

    pub fn id(&self) -> Option<&str> {
        match self {
            Self::Oauth { id, .. } | Self::Command { id, .. } | Self::External { id, .. } => Some(id),
            _ => None,
        }
    }

    /// Methods are keyed by `type` (+ `id` for oauth/command/external); re-registering replaces.
    pub fn same_slot(&self, other: &Method) -> bool {
        self.kind() == other.kind() && self.id() == other.id()
    }

    pub fn form(&self) -> Option<&[Value]> {
        match self {
            Self::Oauth { form, .. } | Self::Key { form, .. } | Self::External { form, .. } => form.as_deref(),
            _ => None,
        }
    }

    /// Wire JSON with upstream key order.
    pub fn to_json(&self) -> Value {
        let mut out = Map::new();
        match self {
            Self::Oauth { id, label, form } | Self::External { id, label, form } => {
                out.insert("id".into(), json!(id));
                out.insert("type".into(), json!(self.kind()));
                out.insert("label".into(), json!(label));
                if let Some(form) = form {
                    out.insert("form".into(), Value::Array(form.clone()));
                }
            }
            Self::Command { id, label, command } => {
                out.insert("id".into(), json!(id));
                out.insert("type".into(), json!("command"));
                out.insert("label".into(), json!(label));
                out.insert("command".into(), json!(command));
            }
            Self::Key { label, form } => {
                out.insert("type".into(), json!("key"));
                if let Some(label) = label {
                    out.insert("label".into(), json!(label));
                }
                if let Some(form) = form {
                    out.insert("form".into(), Value::Array(form.clone()));
                }
            }
            Self::Env { names } => {
                out.insert("type".into(), json!("env"));
                out.insert("names".into(), json!(names));
            }
        }
        Value::Object(out)
    }
}

impl Serialize for Method {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        self.to_json().serialize(serializer)
    }
}

/// Static description of an OAuth method (`Integration.OAuthMethod`).
#[derive(Debug, Clone, PartialEq)]
pub struct OAuthMethodInfo {
    pub id: String,
    pub label: String,
    pub form: Option<Vec<Value>>,
}

impl OAuthMethodInfo {
    pub fn new(id: impl Into<String>, label: impl Into<String>) -> Self {
        Self { id: id.into(), label: label.into(), form: None }
    }
    pub fn with_form(mut self, form: Vec<Value>) -> Self {
        self.form = Some(form);
        self
    }
    pub fn method(&self) -> Method {
        Method::Oauth { id: self.id.clone(), label: self.label.clone(), form: self.form.clone() }
    }
}

// ---------------------------------------------------------------------------------------------
// Connections
// ---------------------------------------------------------------------------------------------

/// `Connection.Status`: runtime problem reported for a connection.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ConnectionStatus {
    /// Always `"needs_auth"`.
    pub status: String,
    pub message: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
}

impl ConnectionStatus {
    pub fn needs_auth(message: impl Into<String>, url: Option<String>) -> Self {
        Self { status: "needs_auth".into(), message: message.into(), url }
    }
}

/// `Connection.Info`.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum ConnectionInfo {
    Credential {
        id: String,
        label: String,
        /// `"key" | "oauth" | "external"`.
        method: String,
        #[serde(default)]
        status: Option<ConnectionStatus>,
    },
    Env {
        name: String,
        #[serde(default)]
        status: Option<ConnectionStatus>,
    },
}

impl ConnectionInfo {
    /// Identity key: `"credential:<id>"` / `"env:<name>"`.
    pub fn key(&self) -> String {
        match self {
            Self::Credential { id, .. } => format!("credential:{id}"),
            Self::Env { name, .. } => format!("env:{name}"),
        }
    }

    fn without_status(&self) -> Self {
        match self {
            Self::Credential { id, label, method, .. } => {
                Self::Credential { id: id.clone(), label: label.clone(), method: method.clone(), status: None }
            }
            Self::Env { name, .. } => Self::Env { name: name.clone(), status: None },
        }
    }

    pub fn to_json(&self) -> Value {
        let mut out = Map::new();
        let status = match self {
            Self::Credential { id, label, method, status } => {
                out.insert("type".into(), json!("credential"));
                out.insert("id".into(), json!(id));
                out.insert("label".into(), json!(label));
                out.insert("method".into(), json!(method));
                status
            }
            Self::Env { name, status } => {
                out.insert("type".into(), json!("env"));
                out.insert("name".into(), json!(name));
                status
            }
        };
        if let Some(status) = status {
            out.insert("status".into(), serde_json::to_value(status).unwrap_or(Value::Null));
        }
        Value::Object(out)
    }
}

impl Serialize for ConnectionInfo {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        self.to_json().serialize(serializer)
    }
}

/// `IntegrationConnection.key(connection | undefined)`.
pub fn connection_key(connection: Option<&ConnectionInfo>) -> Option<String> {
    connection.map(ConnectionInfo::key)
}

/// `Integration.Ref`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct IntegrationRef {
    pub id: String,
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub metadata: Option<Metadata>,
}

/// `Integration.Info`.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct IntegrationInfo {
    pub id: String,
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub metadata: Option<Metadata>,
    pub methods: Vec<Method>,
    pub connections: Vec<ConnectionInfo>,
}

// ---------------------------------------------------------------------------------------------
// OAuth implementation contract
// ---------------------------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AttemptMode {
    Auto,
    Code,
}

/// How an authorization finishes: `auto` runs in the background right away; `code` waits for
/// `oauth.complete {code}`.
pub enum OAuthCallback {
    Auto(BoxFuture<FlowResult<OAuthValue>>),
    Code(Box<dyn FnOnce(String) -> BoxFuture<FlowResult<OAuthValue>> + Send>),
}

/// Result of `authorize(answer)` (`OAuthAuthorization`).
pub struct AuthorizeResult {
    pub url: String,
    pub instructions: String,
    /// Epoch ms; the attempt expires at `expires_at ?? created + 10 min`.
    pub expires_at: Option<u64>,
    pub callback: OAuthCallback,
}

impl AuthorizeResult {
    pub fn auto<F>(url: impl Into<String>, instructions: impl Into<String>, callback: F) -> Self
    where
        F: Future<Output = FlowResult<OAuthValue>> + Send + 'static,
    {
        Self {
            url: url.into(),
            instructions: instructions.into(),
            expires_at: None,
            callback: OAuthCallback::Auto(Box::pin(callback)),
        }
    }

    pub fn mode(&self) -> AttemptMode {
        match self.callback {
            OAuthCallback::Auto(_) => AttemptMode::Auto,
            OAuthCallback::Code(_) => AttemptMode::Code,
        }
    }

    /// Awaits the auto callback (or runs the code callback with `code`). Convenience for direct use.
    pub async fn finish(self, code: Option<String>) -> FlowResult<OAuthValue> {
        match self.callback {
            OAuthCallback::Auto(future) => future.await,
            OAuthCallback::Code(callback) => callback(code.unwrap_or_default()).await,
        }
    }
}

impl std::fmt::Debug for AuthorizeResult {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AuthorizeResult")
            .field("url", &self.url)
            .field("instructions", &self.instructions)
            .field("mode", &self.mode())
            .field("expires_at", &self.expires_at)
            .finish()
    }
}

/// `OAuthImplementation`: one OAuth method of an integration.
#[async_trait]
pub trait OAuthMethod: Send + Sync + 'static {
    fn integration_id(&self) -> String;
    fn info(&self) -> OAuthMethodInfo;
    /// Starts an authorization. Resources (loopback servers) must stop when `scope` is cancelled.
    async fn authorize(&self, answer: FormAnswer, scope: CancellationToken) -> FlowResult<AuthorizeResult>;
    /// Whether [`OAuthMethod::refresh`] is implemented.
    fn can_refresh(&self) -> bool {
        false
    }
    async fn refresh(&self, value: OAuthValue) -> FlowResult<OAuthValue> {
        Ok(value)
    }
    /// Label for a newly connected credential.
    fn label(&self, _value: &OAuthValue) -> Option<String> {
        None
    }
}

// ---------------------------------------------------------------------------------------------
// Attempts
// ---------------------------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct AttemptTime {
    pub created: u64,
    pub expires: u64,
}

/// `Integration.Attempt`.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Attempt {
    #[serde(rename = "attemptID")]
    pub attempt_id: String,
    pub url: String,
    pub instructions: String,
    pub mode: AttemptMode,
    pub time: AttemptTime,
}

/// `Integration.AttemptStatus`.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(tag = "status", rename_all = "lowercase")]
pub enum AttemptStatus {
    Pending { time: AttemptTime },
    Complete { time: AttemptTime },
    Failed { message: String, time: AttemptTime },
    Expired { time: AttemptTime },
}

/// `Integration.CommandAttempt`.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct CommandAttempt {
    #[serde(rename = "attemptID")]
    pub attempt_id: String,
    pub time: AttemptTime,
}

/// `Integration.CommandAttemptStatus`.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(tag = "status", rename_all = "lowercase")]
pub enum CommandAttemptStatus {
    Pending {
        #[serde(skip_serializing_if = "Option::is_none")]
        message: Option<String>,
        time: AttemptTime,
    },
    Complete {
        time: AttemptTime,
    },
    Failed {
        message: String,
        time: AttemptTime,
    },
    Expired {
        time: AttemptTime,
    },
}

/// Integration errors.
#[derive(Debug, Clone, PartialEq, thiserror::Error)]
pub enum IntegrationError {
    /// `Integration.Authorization`: message = cause message or `"Authorization failed"`.
    #[error("{0}")]
    Authorization(String),
    /// `Integration.CodeRequired`.
    #[error("Authorization code required")]
    CodeRequired { attempt_id: String },
    /// `Integration.AttemptNotFound`.
    #[error("Attempt not found: {attempt_id}")]
    AttemptNotFound { integration_id: String, attempt_id: String },
    /// Programming/storage errors (upstream defects).
    #[error("{0}")]
    Defect(String),
}

impl IntegrationError {
    /// `AuthorizationError` from a cause message (empty → `"Authorization failed"`).
    pub fn authorization(message: impl Into<String>) -> Self {
        let message = message.into();
        Self::Authorization(if message.is_empty() { "Authorization failed".into() } else { message })
    }

    pub fn tag(&self) -> &'static str {
        match self {
            Self::Authorization(_) => "Integration.Authorization",
            Self::CodeRequired { .. } => "Integration.CodeRequired",
            Self::AttemptNotFound { .. } => "Integration.AttemptNotFound",
            Self::Defect(_) => "Defect",
        }
    }
}

fn defect(error: impl std::fmt::Display) -> IntegrationError {
    IntegrationError::Defect(error.to_string())
}

enum OAuthAttempt {
    Pending {
        completing: bool,
        persisting: bool,
        mode: AttemptMode,
        code_callback: Option<Box<dyn FnOnce(String) -> BoxFuture<FlowResult<OAuthValue>> + Send>>,
        integration_id: String,
        method_id: String,
        label: Option<String>,
        scope: CancellationToken,
        time: AttemptTime,
    },
    Terminal(Terminal),
}

struct Terminal {
    status: TerminalStatus,
    integration_id: String,
    message: Option<String>,
    remove_at: u64,
    time: AttemptTime,
}

#[derive(Clone, Copy, PartialEq)]
enum TerminalStatus {
    Complete,
    Failed,
    Expired,
}

enum CommandAttemptState {
    Pending {
        integration_id: String,
        label: Option<String>,
        message: Option<String>,
        persisting: bool,
        scope: CancellationToken,
        time: AttemptTime,
    },
    Terminal(Terminal),
}

/// Integration registry events.
#[derive(Debug, Clone, PartialEq)]
pub enum IntegrationEvent {
    /// `integration.updated`
    Updated,
}

struct Entry {
    reference: IntegrationRef,
    methods: Vec<Method>,
    oauth: HashMap<String, Arc<dyn OAuthMethod>>,
}

impl Entry {
    fn new(id: &str) -> Self {
        Self {
            reference: IntegrationRef { id: id.into(), name: id.into(), metadata: None },
            methods: Vec::new(),
            oauth: HashMap::new(),
        }
    }
}

/// The integration service (`Integration.Interface`): registry, connections, OAuth and command attempts.
pub struct Integrations {
    store: Arc<dyn CredentialStore>,
    env: EnvSource,
    clock: Clock,
    registry: RwLock<IndexMap<String, Entry>>,
    revision: Mutex<u64>,
    attempts: Mutex<HashMap<String, OAuthAttempt>>,
    command_attempts: Mutex<HashMap<String, CommandAttemptState>>,
    statuses: Mutex<HashMap<String, ConnectionStatus>>,
    events: broadcast::Sender<IntegrationEvent>,
}

/// Options for [`Integrations::connect_oauth`].
#[derive(Debug, Clone, Default)]
pub struct OAuthConnect {
    pub integration_id: String,
    pub method_id: String,
    pub answer: Option<FormAnswer>,
    pub label: Option<String>,
}

impl Integrations {
    pub fn new(store: Arc<dyn CredentialStore>) -> Arc<Self> {
        Self::with(store, super::process_env(), super::system_clock())
    }

    pub fn with(store: Arc<dyn CredentialStore>, env: EnvSource, clock: Clock) -> Arc<Self> {
        Arc::new(Self {
            store,
            env,
            clock,
            registry: RwLock::new(IndexMap::new()),
            revision: Mutex::new(0),
            attempts: Mutex::new(HashMap::new()),
            command_attempts: Mutex::new(HashMap::new()),
            statuses: Mutex::new(HashMap::new()),
            events: broadcast::channel(64).0,
        })
    }

    pub fn store(&self) -> &Arc<dyn CredentialStore> {
        &self.store
    }

    pub fn subscribe(&self) -> broadcast::Receiver<IntegrationEvent> {
        self.events.subscribe()
    }

    fn now(&self) -> u64 {
        (self.clock)()
    }

    fn changed(&self) {
        *self.revision.lock() += 1;
        let _ = self.events.send(IntegrationEvent::Updated);
    }

    /// Revision of integration definitions.
    pub fn revision(&self) -> u64 {
        *self.revision.lock()
    }

    // ---- registry editing (`Editor`) --------------------------------------------------------

    /// Creates (if needed) and edits an integration's ref; the id is kept.
    pub fn update(&self, id: &str, edit: impl FnOnce(&mut IntegrationRef)) {
        {
            let mut registry = self.registry.write();
            let entry = registry.entry(id.to_string()).or_insert_with(|| Entry::new(id));
            edit(&mut entry.reference);
            entry.reference.id = id.to_string();
        }
        self.changed();
    }

    pub fn remove(&self, id: &str) {
        self.registry.write().shift_remove(id);
        self.changed();
    }

    pub fn refs(&self) -> Vec<IntegrationRef> {
        self.registry.read().values().map(|e| e.reference.clone()).collect()
    }

    pub fn methods(&self, integration_id: &str) -> Vec<Method> {
        self.registry.read().get(integration_id).map(|e| e.methods.clone()).unwrap_or_default()
    }

    /// Registers (or replaces) a non-OAuth method.
    pub fn register_method(&self, integration_id: &str, method: Method) {
        {
            let mut registry = self.registry.write();
            let entry = registry.entry(integration_id.to_string()).or_insert_with(|| Entry::new(integration_id));
            match entry.methods.iter().position(|m| m.same_slot(&method)) {
                Some(index) => entry.methods[index] = method,
                None => entry.methods.push(method),
            }
        }
        self.changed();
    }

    /// Registers (or replaces) an OAuth method with its implementation.
    pub fn register_oauth(&self, implementation: Arc<dyn OAuthMethod>) {
        let integration_id = implementation.integration_id();
        let info = implementation.info();
        let method = info.method();
        {
            let mut registry = self.registry.write();
            let entry = registry.entry(integration_id.clone()).or_insert_with(|| Entry::new(&integration_id));
            match entry.methods.iter().position(|m| m.same_slot(&method)) {
                Some(index) => entry.methods[index] = method,
                None => entry.methods.push(method),
            }
            entry.oauth.insert(info.id, implementation);
        }
        self.changed();
    }

    pub fn remove_method(&self, integration_id: &str, method: &Method) {
        {
            let mut registry = self.registry.write();
            let Some(entry) = registry.get_mut(integration_id) else { return };
            if let Some(index) = entry.methods.iter().position(|m| m.same_slot(method)) {
                entry.methods.remove(index);
            }
            if let Method::Oauth { id, .. } = method {
                entry.oauth.remove(id);
            }
        }
        self.changed();
    }

    fn implementation(&self, integration_id: &str, method_id: &str) -> Option<Arc<dyn OAuthMethod>> {
        self.registry.read().get(integration_id).and_then(|e| e.oauth.get(method_id).cloned())
    }

    // ---- projection ------------------------------------------------------------------------

    /// Stored credentials (newest selection first) then env connections whose variable is set.
    fn resolve_connections(&self, methods: &[Method], saved: &[Credential]) -> Vec<ConnectionInfo> {
        let mut out: Vec<ConnectionInfo> = saved
            .iter()
            .rev()
            .map(|c| ConnectionInfo::Credential {
                id: c.id.clone(),
                label: c.label.clone(),
                method: c.value.kind().into(),
                status: None,
            })
            .collect();
        for method in methods {
            if let Method::Env { names } = method {
                for name in names {
                    if (self.env)(name).is_some_and(|v| !v.is_empty()) {
                        out.push(ConnectionInfo::Env { name: name.clone(), status: None });
                    }
                }
            }
        }
        out
    }

    fn status_key(integration_id: &str, connection: &ConnectionInfo) -> String {
        format!("{integration_id}:{}", connection.key())
    }

    fn project(&self, reference: &IntegrationRef, methods: Vec<Method>, connections: Vec<ConnectionInfo>) -> IntegrationInfo {
        let statuses = self.statuses.lock();
        IntegrationInfo {
            id: reference.id.clone(),
            name: reference.name.clone(),
            metadata: reference.metadata.clone(),
            methods,
            connections: connections
                .into_iter()
                .map(|connection| {
                    match statuses.get(&Self::status_key(&reference.id, &connection)).cloned() {
                        None => connection,
                        Some(status) => match connection {
                            ConnectionInfo::Credential { id, label, method, .. } => {
                                ConnectionInfo::Credential { id, label, method, status: Some(status) }
                            }
                            ConnectionInfo::Env { name, .. } => ConnectionInfo::Env { name, status: Some(status) },
                        },
                    }
                })
                .collect(),
        }
    }

    /// One integration with its methods and current connections.
    pub async fn get(&self, id: &str) -> Result<Option<IntegrationInfo>, IntegrationError> {
        let Some((reference, methods)) =
            self.registry.read().get(id).map(|e| (e.reference.clone(), e.methods.clone()))
        else {
            return Ok(None);
        };
        let saved = self.store.list(id).await.map_err(defect)?;
        let connections = self.resolve_connections(&methods, &saved);
        Ok(Some(self.project(&reference, methods, connections)))
    }

    /// All integrations sorted by name.
    pub async fn list(&self) -> Result<Vec<IntegrationInfo>, IntegrationError> {
        let all = self.store.all().await.map_err(defect)?;
        let entries: Vec<(IntegrationRef, Vec<Method>)> =
            self.registry.read().values().map(|e| (e.reference.clone(), e.methods.clone())).collect();
        let mut out: Vec<IntegrationInfo> = entries
            .into_iter()
            .map(|(reference, methods)| {
                let saved: Vec<Credential> =
                    all.iter().filter(|c| c.integration_id == reference.id).cloned().collect();
                let connections = self.resolve_connections(&methods, &saved);
                self.project(&reference, methods, connections)
            })
            .collect();
        out.sort_by(|a, b| locale_compare(&a.name, &b.name));
        Ok(out)
    }

    // ---- connections -----------------------------------------------------------------------

    /// The active connection: stored credentials (selected first, then newest) beat env connections.
    pub async fn active(&self, id: &str) -> Result<Option<ConnectionInfo>, IntegrationError> {
        let methods = self.methods(id);
        let saved = self.store.list(id).await.map_err(defect)?;
        Ok(self.resolve_connections(&methods, &saved).into_iter().next())
    }

    /// Resolves a connection into credential material, refreshing OAuth credentials that expire within 5 min.
    pub async fn resolve(&self, connection: &ConnectionInfo) -> Result<Option<CredentialValue>, IntegrationError> {
        let id = match connection {
            ConnectionInfo::Env { name, .. } => {
                return Ok((self.env)(name).filter(|v| !v.is_empty()).map(CredentialValue::key));
            }
            ConnectionInfo::Credential { id, .. } => id,
        };
        let Some(credential) = self.store.get(id).await.map_err(defect)? else { return Ok(None) };
        let Some(oauth) = credential.value.as_oauth() else { return Ok(Some(credential.value)) };
        let Some(implementation) = self.implementation(&credential.integration_id, &oauth.method_id) else {
            return Ok(Some(credential.value));
        };
        if !implementation.can_refresh() {
            return Ok(Some(credential.value));
        }
        if oauth.expires > self.now() + REFRESH_WINDOW_MS {
            return Ok(Some(credential.value));
        }
        let value = implementation
            .refresh(oauth)
            .await
            .map_err(|e| IntegrationError::authorization(e.message))?
            .into_value();
        self.store
            .update(&credential.id, CredentialUpdate { label: None, value: Some(value.clone()) })
            .await
            .map_err(defect)?;
        Ok(Some(value))
    }

    /// `createCredential`: an explicit label wins, otherwise a unique `"<name>"`, `"<name> 2"`, ... label.
    pub async fn create_credential(&self, mut input: CreateCredential) -> Result<Credential, IntegrationError> {
        if input.label.is_none() {
            let name = self
                .registry
                .read()
                .get(&input.integration_id)
                .map(|e| e.reference.name.clone())
                .unwrap_or_else(|| input.integration_id.clone());
            let labels: Vec<String> = self
                .store
                .list(&input.integration_id)
                .await
                .map_err(defect)?
                .into_iter()
                .map(|c| c.label)
                .collect();
            input.label = Some(unique_label(&name, &labels));
        }
        self.store.create(input).await.map_err(defect)
    }

    /// `connection.key`: stores `{type:"key", key, configuration?: answer}`.
    pub async fn connect_key(
        &self,
        integration_id: &str,
        key: &str,
        answer: Option<FormAnswer>,
        label: Option<String>,
    ) -> Result<(), IntegrationError> {
        let method = self
            .methods(integration_id)
            .into_iter()
            .find(|m| matches!(m, Method::Key { .. }))
            .ok_or_else(|| IntegrationError::Defect(format!("Key method not found: {integration_id}")))?;
        let answer = answer.unwrap_or_default();
        if let Some(invalid) = form::check(method.form(), &answer) {
            return Err(IntegrationError::authorization(invalid));
        }
        if method.form().is_none() && !answer.is_empty() {
            return Err(IntegrationError::authorization("Key method does not accept a form answer"));
        }
        let value = CredentialValue::Key {
            key: key.to_string(),
            metadata: None,
            configuration: (!answer.is_empty()).then_some(answer),
        };
        let mut input = CreateCredential::new(integration_id, value);
        input.label = label;
        self.create_credential(input).await.map(|_| ())
    }

    /// `connection.external`: stores `{type:"external", methodID, metadata?: answer}`.
    pub async fn connect_external(
        &self,
        integration_id: &str,
        method_id: &str,
        answer: Option<FormAnswer>,
        label: Option<String>,
    ) -> Result<(), IntegrationError> {
        let method = self
            .methods(integration_id)
            .into_iter()
            .find(|m| matches!(m, Method::External { id, .. } if id == method_id))
            .ok_or_else(|| IntegrationError::authorization(format!("External method not found: {method_id}")))?;
        let answer = answer.unwrap_or_default();
        if let Some(invalid) = form::check(method.form(), &answer) {
            return Err(IntegrationError::authorization(invalid));
        }
        if method.form().is_none() && !answer.is_empty() {
            return Err(IntegrationError::authorization("External method does not accept a form answer"));
        }
        let value = CredentialValue::External {
            method_id: method_id.to_string(),
            metadata: (!answer.is_empty()).then_some(answer),
        };
        let mut input = CreateCredential::new(integration_id, value);
        input.label = label;
        self.create_credential(input).await.map(|_| ())
    }

    pub async fn activate(&self, credential_id: &str) -> Result<(), IntegrationError> {
        self.store.activate(credential_id).await.map_err(defect)
    }

    pub async fn update_label(&self, credential_id: &str, label: Option<String>) -> Result<(), IntegrationError> {
        self.store.update(credential_id, CredentialUpdate { label, value: None }).await.map_err(defect)
    }

    pub async fn remove_credential(&self, credential_id: &str) -> Result<(), IntegrationError> {
        self.store.remove(credential_id).await.map_err(defect)
    }

    /// Reports or clears a runtime problem with a connection.
    pub fn set_status(&self, integration_id: &str, connection: &ConnectionInfo, status: Option<ConnectionStatus>) {
        let key = Self::status_key(integration_id, &connection.without_status());
        {
            let mut statuses = self.statuses.lock();
            if statuses.get(&key) == status.as_ref() {
                return;
            }
            match status {
                Some(status) => statuses.insert(key, status),
                None => statuses.remove(&key),
            };
        }
        let _ = self.events.send(IntegrationEvent::Updated);
    }

    // ---- OAuth attempts --------------------------------------------------------------------

    /// `oauth.connect`: validates the form, runs `authorize`, records a pending attempt and, in `auto` mode,
    /// starts the callback in the background.
    pub async fn connect_oauth(self: &Arc<Self>, input: OAuthConnect) -> Result<Attempt, IntegrationError> {
        let implementation = self.implementation(&input.integration_id, &input.method_id).ok_or_else(|| {
            IntegrationError::Defect(format!("OAuth method not found: {}/{}", input.integration_id, input.method_id))
        })?;
        let answer = input.answer.unwrap_or_default();
        if let Some(invalid) = form::check(implementation.info().form.as_deref(), &answer) {
            return Err(IntegrationError::authorization(invalid));
        }
        let scope = CancellationToken::new();
        let authorization = match implementation.authorize(answer, scope.clone()).await {
            Ok(authorization) => authorization,
            Err(error) => {
                scope.cancel();
                return Err(IntegrationError::authorization(error.message));
            }
        };
        let id = oc_schema::id::connection();
        let created = self.now();
        let time = AttemptTime { created, expires: authorization.expires_at.unwrap_or(created + ATTEMPT_LIFETIME_MS) };
        let mode = authorization.mode();
        let (auto, code_callback) = match authorization.callback {
            OAuthCallback::Auto(future) => (Some(future), None),
            OAuthCallback::Code(callback) => (None, Some(callback)),
        };
        self.attempts.lock().insert(
            id.clone(),
            OAuthAttempt::Pending {
                completing: mode == AttemptMode::Auto,
                persisting: false,
                mode,
                code_callback,
                integration_id: input.integration_id.clone(),
                method_id: input.method_id.clone(),
                label: input.label,
                scope: scope.clone(),
                time,
            },
        );
        if let Some(future) = auto {
            let this = Arc::clone(self);
            let attempt_id = id.clone();
            tokio::spawn(async move {
                let result = tokio::select! {
                    _ = scope.cancelled() => return,
                    result = future => result,
                };
                let _ = this.settle(&attempt_id, result).await;
            });
        }
        Ok(Attempt { attempt_id: id, url: authorization.url, instructions: authorization.instructions, mode, time })
    }

    /// Settles a pending attempt with the callback's result; on success persists the credential.
    async fn settle(&self, attempt_id: &str, result: FlowResult<OAuthValue>) -> Result<(), IntegrationError> {
        let now = self.now();
        let claimed = {
            let mut attempts = self.attempts.lock();
            let Some(OAuthAttempt::Pending { persisting, integration_id, method_id, label, scope, time, .. }) =
                attempts.get_mut(attempt_id)
            else {
                return Ok(());
            };
            if *persisting {
                return Ok(());
            }
            match &result {
                Ok(_) => {
                    *persisting = true;
                    (integration_id.clone(), method_id.clone(), label.clone(), scope.clone(), *time)
                }
                Err(error) => {
                    let terminal = Terminal {
                        status: TerminalStatus::Failed,
                        integration_id: integration_id.clone(),
                        message: Some(error.message.clone()),
                        remove_at: now + TERMINAL_RETENTION_MS,
                        time: *time,
                    };
                    let scope = scope.clone();
                    attempts.insert(attempt_id.to_string(), OAuthAttempt::Terminal(terminal));
                    scope.cancel();
                    return Ok(());
                }
            }
        };
        let (integration_id, method_id, label, scope, time) = claimed;
        let value = result.expect("checked above");
        let label = label.or_else(|| self.implementation(&integration_id, &method_id).and_then(|i| i.label(&value)));
        let mut input = CreateCredential::new(&integration_id, value.into_value());
        input.label = label;
        let persistence = self.create_credential(input).await;
        let settled = self.now();
        let terminal = Terminal {
            status: if persistence.is_ok() { TerminalStatus::Complete } else { TerminalStatus::Failed },
            integration_id,
            message: persistence.as_ref().err().map(ToString::to_string),
            remove_at: settled + TERMINAL_RETENTION_MS,
            time,
        };
        self.attempts.lock().insert(attempt_id.to_string(), OAuthAttempt::Terminal(terminal));
        scope.cancel();
        persistence.map(|_| ())
    }

    /// `oauth.status`.
    pub fn oauth_status(&self, integration_id: &str, attempt_id: &str) -> Result<AttemptStatus, IntegrationError> {
        let attempts = self.attempts.lock();
        let not_found = || IntegrationError::AttemptNotFound {
            integration_id: integration_id.into(),
            attempt_id: attempt_id.into(),
        };
        match attempts.get(attempt_id).ok_or_else(not_found)? {
            OAuthAttempt::Pending { integration_id: owner, time, .. } => {
                if owner != integration_id {
                    return Err(not_found());
                }
                Ok(AttemptStatus::Pending { time: *time })
            }
            OAuthAttempt::Terminal(t) => {
                if t.integration_id != integration_id {
                    return Err(not_found());
                }
                Ok(match t.status {
                    TerminalStatus::Complete => AttemptStatus::Complete { time: t.time },
                    TerminalStatus::Expired => AttemptStatus::Expired { time: t.time },
                    TerminalStatus::Failed => AttemptStatus::Failed {
                        message: t.message.clone().unwrap_or_else(|| "Authorization failed".into()),
                        time: t.time,
                    },
                })
            }
        }
    }

    /// `oauth.complete`: runs a `code`-mode callback and stores its credential.
    pub async fn complete_oauth(
        &self,
        integration_id: &str,
        attempt_id: &str,
        code: Option<String>,
    ) -> Result<(), IntegrationError> {
        let callback = {
            let mut attempts = self.attempts.lock();
            let not_found = || IntegrationError::AttemptNotFound {
                integration_id: integration_id.into(),
                attempt_id: attempt_id.into(),
            };
            match attempts.get_mut(attempt_id) {
                None => return Err(not_found()),
                Some(OAuthAttempt::Terminal(t)) => {
                    if t.integration_id != integration_id {
                        return Err(not_found());
                    }
                    return Ok(());
                }
                Some(OAuthAttempt::Pending { integration_id: owner, mode, completing, code_callback, .. }) => {
                    if owner != integration_id {
                        return Err(not_found());
                    }
                    if *mode == AttemptMode::Code && code.is_none() {
                        return Err(IntegrationError::CodeRequired { attempt_id: attempt_id.into() });
                    }
                    if *completing {
                        return Err(IntegrationError::Defect(format!(
                            "OAuth attempt already completing: {attempt_id}"
                        )));
                    }
                    *completing = true;
                    code_callback.take()
                }
            }
        };
        let Some(callback) = callback else {
            return Err(IntegrationError::Defect(format!("OAuth attempt already completing: {attempt_id}")));
        };
        let result = callback(code.unwrap_or_default())
            .await
            .map_err(|e| FlowError::new(IntegrationError::authorization(e.message).to_string()));
        let failure = result.as_ref().err().cloned();
        self.settle(attempt_id, result).await?;
        match failure {
            Some(error) => Err(IntegrationError::Authorization(error.message)),
            None => Ok(()),
        }
    }

    /// `oauth.cancel`: removes a pending attempt and closes its scope (stops loopback servers).
    pub fn cancel_oauth(&self, integration_id: &str, attempt_id: &str) {
        let mut attempts = self.attempts.lock();
        let scope = match attempts.get(attempt_id) {
            Some(OAuthAttempt::Pending { integration_id: owner, persisting: false, scope, .. })
                if owner == integration_id =>
            {
                scope.clone()
            }
            _ => return,
        };
        attempts.remove(attempt_id);
        scope.cancel();
    }

    /// Expires pending attempts past their deadline and drops terminal attempts past retention.
    pub fn scrub(&self) {
        let now = self.now();
        let mut attempts = self.attempts.lock();
        let ids: Vec<String> = attempts.keys().cloned().collect();
        for id in ids {
            let expire = match attempts.get(&id) {
                Some(OAuthAttempt::Pending { persisting: false, time, integration_id, scope, .. })
                    if time.expires <= now =>
                {
                    Some((integration_id.clone(), *time, scope.clone()))
                }
                Some(OAuthAttempt::Terminal(t)) if t.remove_at <= now => {
                    attempts.remove(&id);
                    None
                }
                _ => None,
            };
            if let Some((integration_id, time, scope)) = expire {
                attempts.insert(
                    id,
                    OAuthAttempt::Terminal(Terminal {
                        status: TerminalStatus::Expired,
                        integration_id,
                        message: None,
                        remove_at: now + TERMINAL_RETENTION_MS,
                        time,
                    }),
                );
                scope.cancel();
            }
        }
        drop(attempts);
        let mut commands = self.command_attempts.lock();
        let ids: Vec<String> = commands.keys().cloned().collect();
        for id in ids {
            let expire = match commands.get(&id) {
                Some(CommandAttemptState::Pending { persisting: false, time, integration_id, scope, .. })
                    if time.expires <= now =>
                {
                    Some((integration_id.clone(), *time, scope.clone()))
                }
                Some(CommandAttemptState::Terminal(t)) if t.remove_at <= now => {
                    commands.remove(&id);
                    None
                }
                _ => None,
            };
            if let Some((integration_id, time, scope)) = expire {
                commands.insert(
                    id,
                    CommandAttemptState::Terminal(Terminal {
                        status: TerminalStatus::Expired,
                        integration_id,
                        message: None,
                        remove_at: now + TERMINAL_RETENTION_MS,
                        time,
                    }),
                );
                scope.cancel();
            }
        }
    }

    /// Starts the 30 s scrubber; it stops when the service is dropped.
    pub fn spawn_scrubber(self: &Arc<Self>) -> tokio::task::JoinHandle<()> {
        let weak: Weak<Self> = Arc::downgrade(self);
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(std::time::Duration::from_millis(SCRUB_INTERVAL_MS));
            interval.tick().await;
            loop {
                interval.tick().await;
                match weak.upgrade() {
                    Some(this) => this.scrub(),
                    None => break,
                }
            }
        })
    }

    // ---- command attempts ------------------------------------------------------------------

    /// `command.connect`: spawns the method's command (stdin ignored, env inherited); stderr becomes the
    /// status message; exit 0 with non-empty trimmed stdout stores a key credential.
    pub async fn connect_command(
        self: &Arc<Self>,
        integration_id: &str,
        method_id: &str,
        label: Option<String>,
    ) -> Result<CommandAttempt, IntegrationError> {
        let command = self
            .methods(integration_id)
            .into_iter()
            .find_map(|m| match m {
                Method::Command { id, command, .. } if id == method_id => Some(command),
                _ => None,
            })
            .filter(|command| command.first().is_some_and(|c| !c.is_empty()))
            .ok_or_else(|| {
                IntegrationError::Defect(format!("Command method not found: {integration_id}/{method_id}"))
            })?;
        let scope = CancellationToken::new();
        let attempt_id = oc_schema::id::connection();
        let created = self.now();
        let time = AttemptTime { created, expires: created + ATTEMPT_LIFETIME_MS };
        self.command_attempts.lock().insert(
            attempt_id.clone(),
            CommandAttemptState::Pending {
                integration_id: integration_id.to_string(),
                label,
                message: None,
                persisting: false,
                scope: scope.clone(),
                time,
            },
        );
        let this = Arc::clone(self);
        let id = attempt_id.clone();
        tokio::spawn(async move {
            let run = this.run_command(&id, &command);
            let result = tokio::select! {
                _ = scope.cancelled() => return,
                result = run => result,
            };
            this.settle_command(&id, result).await;
        });
        Ok(CommandAttempt { attempt_id, time })
    }

    async fn run_command(&self, attempt_id: &str, command: &[String]) -> Result<String, String> {
        use tokio::io::AsyncReadExt;
        let mut child = tokio::process::Command::new(&command[0])
            .args(&command[1..])
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .map_err(|e| e.to_string())?;
        let mut stdout = child.stdout.take().expect("piped stdout");
        let mut stderr = child.stderr.take().expect("piped stderr");
        let collect = tokio::spawn(async move {
            let mut buffer = Vec::new();
            let _ = stdout.read_to_end(&mut buffer).await;
            buffer
        });
        let mut chunk = [0u8; 4096];
        let mut pending: Vec<u8> = Vec::new();
        loop {
            let read = stderr.read(&mut chunk).await.unwrap_or(0);
            if read == 0 {
                break;
            }
            pending.extend_from_slice(&chunk[..read]);
            // Decode complete UTF-8 prefixes (Stream.decodeText).
            let valid = match std::str::from_utf8(&pending) {
                Ok(_) => pending.len(),
                Err(e) => e.valid_up_to(),
            };
            if valid == 0 {
                continue;
            }
            let text = String::from_utf8_lossy(&pending[..valid]).to_string();
            pending.drain(..valid);
            let mut attempts = self.command_attempts.lock();
            if let Some(CommandAttemptState::Pending { message, .. }) = attempts.get_mut(attempt_id) {
                message.get_or_insert_with(String::new).push_str(&text);
            }
        }
        let status = child.wait().await.map_err(|e| e.to_string())?;
        if !status.success() {
            let message = match self.command_attempts.lock().get(attempt_id) {
                Some(CommandAttemptState::Pending { message, .. }) => message.clone(),
                _ => None,
            };
            let code = status.code().map(|c| c.to_string()).unwrap_or_else(|| "null".into());
            return Err(message
                .map(|m| m.trim().to_string())
                .filter(|m| !m.is_empty())
                .unwrap_or_else(|| format!("Authentication command exited {code}")));
        }
        let stdout = collect.await.unwrap_or_default();
        let credential = String::from_utf8_lossy(&stdout).trim().to_string();
        if credential.is_empty() {
            return Err("Authentication command returned no credential".into());
        }
        Ok(credential)
    }

    async fn settle_command(&self, attempt_id: &str, result: Result<String, String>) {
        let now = self.now();
        let claimed = {
            let mut attempts = self.command_attempts.lock();
            let Some(CommandAttemptState::Pending { persisting, integration_id, label, scope, time, .. }) =
                attempts.get_mut(attempt_id)
            else {
                return;
            };
            if *persisting {
                return;
            }
            match &result {
                Ok(_) => {
                    *persisting = true;
                    (integration_id.clone(), label.clone(), scope.clone(), *time)
                }
                Err(message) => {
                    let terminal = Terminal {
                        status: TerminalStatus::Failed,
                        integration_id: integration_id.clone(),
                        message: Some(message.clone()),
                        remove_at: now + TERMINAL_RETENTION_MS,
                        time: *time,
                    };
                    let scope = scope.clone();
                    attempts.insert(attempt_id.to_string(), CommandAttemptState::Terminal(terminal));
                    scope.cancel();
                    return;
                }
            }
        };
        let (integration_id, label, scope, time) = claimed;
        let mut input = CreateCredential::new(&integration_id, CredentialValue::key(result.expect("checked above")));
        input.label = label;
        let persistence = self.create_credential(input).await;
        let settled = self.now();
        let terminal = Terminal {
            status: if persistence.is_ok() { TerminalStatus::Complete } else { TerminalStatus::Failed },
            integration_id,
            message: persistence.err().map(|e| e.to_string()),
            remove_at: settled + TERMINAL_RETENTION_MS,
            time,
        };
        self.command_attempts.lock().insert(attempt_id.to_string(), CommandAttemptState::Terminal(terminal));
        scope.cancel();
    }

    /// `command.status`.
    pub fn command_status(
        &self,
        integration_id: &str,
        attempt_id: &str,
    ) -> Result<CommandAttemptStatus, IntegrationError> {
        let attempts = self.command_attempts.lock();
        let not_found = || IntegrationError::AttemptNotFound {
            integration_id: integration_id.into(),
            attempt_id: attempt_id.into(),
        };
        match attempts.get(attempt_id).ok_or_else(not_found)? {
            CommandAttemptState::Pending { integration_id: owner, message, time, .. } => {
                if owner != integration_id {
                    return Err(not_found());
                }
                Ok(CommandAttemptStatus::Pending {
                    message: message.clone().filter(|m| !m.is_empty()),
                    time: *time,
                })
            }
            CommandAttemptState::Terminal(t) => {
                if t.integration_id != integration_id {
                    return Err(not_found());
                }
                Ok(match t.status {
                    TerminalStatus::Complete => CommandAttemptStatus::Complete { time: t.time },
                    TerminalStatus::Expired => CommandAttemptStatus::Expired { time: t.time },
                    TerminalStatus::Failed => CommandAttemptStatus::Failed {
                        message: t.message.clone().unwrap_or_else(|| "Authentication failed".into()),
                        time: t.time,
                    },
                })
            }
        }
    }

    /// `command.cancel`.
    pub fn cancel_command(&self, integration_id: &str, attempt_id: &str) {
        let mut attempts = self.command_attempts.lock();
        let scope = match attempts.get(attempt_id) {
            Some(CommandAttemptState::Pending { integration_id: owner, persisting: false, scope, .. })
                if owner == integration_id =>
            {
                scope.clone()
            }
            _ => return,
        };
        attempts.remove(attempt_id);
        scope.cancel();
    }
}

/// Approximation of `String.prototype.localeCompare` for names: case-insensitive first, then exact.
pub fn locale_compare(a: &str, b: &str) -> std::cmp::Ordering {
    a.to_lowercase().cmp(&b.to_lowercase()).then_with(|| b.cmp(a))
}
