//! JSON-RPC client over a [`Transport`] — the parts of the SDK `Protocol`/`Client` classes that
//! upstream relies on: request/response correlation with timeouts and cancellation, the legacy
//! `initialize` handshake, `2026-07-28` era negotiation (`server/discover` probe, `_meta`
//! envelope, `subscriptions/listen`, `input_required` auto-fulfilment), server-initiated
//! requests (`ping`, `roots/list`, `elicitation/create`), list-changed notifications and
//! paginated listing.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};
use std::time::Duration;

use async_trait::async_trait;
use parking_lot::{Mutex, RwLock};
use serde_json::{Map, Value, json};
use tokio::sync::{mpsc, oneshot};
use tokio_util::sync::CancellationToken;

use crate::config::Protocol as ProtocolSetting;
use crate::error::{McpError, SdkErrorCode, codes};
use crate::jsonrpc;
use crate::transport::{SendOptions, Transport, TransportEvent};
use crate::types::ClientInfo;

/// `LATEST_PROTOCOL_VERSION` — what the legacy handshake offers.
pub const LATEST_PROTOCOL_VERSION: &str = "2025-11-25";
/// `SUPPORTED_PROTOCOL_VERSIONS` (legacy era).
pub const SUPPORTED_PROTOCOL_VERSIONS: [&str; 5] = ["2025-11-25", "2025-06-18", "2025-03-26", "2024-11-05", "2024-10-07"];
/// First (and only) modern revision.
pub const MODERN_PROTOCOL_VERSION: &str = "2026-07-28";
/// SDK `DEFAULT_REQUEST_TIMEOUT_MSEC`.
pub const DEFAULT_REQUEST_TIMEOUT: Duration = Duration::from_secs(60);
const DEFAULT_LIST_MAX_PAGES: usize = 64;
const INPUT_REQUIRED_MAX_ROUNDS: usize = 10;
const REQUEST_STATE_ONLY_LEG_PACING: Duration = Duration::from_millis(250);

pub const PROTOCOL_VERSION_META_KEY: &str = "io.modelcontextprotocol/protocolVersion";
pub const CLIENT_INFO_META_KEY: &str = "io.modelcontextprotocol/clientInfo";
pub const CLIENT_CAPABILITIES_META_KEY: &str = "io.modelcontextprotocol/clientCapabilities";
pub const SERVER_INFO_META_KEY: &str = "io.modelcontextprotocol/serverInfo";
pub const SUBSCRIPTION_ID_META_KEY: &str = "io.modelcontextprotocol/subscriptionId";

pub fn is_modern(version: &str) -> bool {
    version >= MODERN_PROTOCOL_VERSION
}

/// Handles server elicitation requests (core turns them into Location-scoped forms owned by the
/// `"global"` session).
#[async_trait]
pub trait ElicitationHandler: Send + Sync {
    /// `elicitation/create`: `params` is the request params with `mode` defaulted to `"form"`.
    /// Returns an `ElicitResult` (`{action: "accept"|"decline"|"cancel", content?}`).
    async fn create(&self, server: &str, params: Value, cancel: CancellationToken) -> Result<Value, String>;
    /// Legacy `notifications/elicitation/complete` for a URL-mode elicitation.
    async fn complete(&self, server: &str, elicitation_id: &str);
}

/// Options for a single request.
#[derive(Clone, Default)]
pub struct RequestOptions {
    pub timeout: Option<Duration>,
    pub cancel: Option<CancellationToken>,
    /// Request progress notifications (`onprogress`): adds `_meta.progressToken`.
    pub progress: bool,
}

type Callback = Arc<dyn Fn() + Send + Sync>;

#[derive(Default)]
struct Callbacks {
    on_error: Option<Arc<dyn Fn(&McpError) + Send + Sync>>,
    on_close: Option<Callback>,
    tools: Option<Callback>,
    prompts: Option<Callback>,
    resources: Option<Callback>,
}

pub struct ClientConfig {
    pub server: String,
    pub client_info: ClientInfo,
    pub negotiation: Option<ProtocolSetting>,
    pub elicitation: Option<Arc<dyn ElicitationHandler>>,
    /// `file://` URI of the Location directory returned from `roots/list`.
    pub root_uri: String,
}

enum Pending {
    Response(oneshot::Sender<Value>),
}

pub struct Client {
    transport: Arc<dyn Transport>,
    config: ClientConfig,
    capabilities: Value,
    next_id: AtomicI64,
    pending: Mutex<HashMap<String, Pending>>,
    handlers: Mutex<HashMap<String, CancellationToken>>,
    progress: Mutex<std::collections::HashSet<i64>>,
    connected: AtomicBool,
    server_capabilities: RwLock<Option<Value>>,
    server_info: RwLock<Option<Value>>,
    instructions: RwLock<Option<String>>,
    negotiated: RwLock<Option<String>>,
    callbacks: RwLock<Callbacks>,
    list_changed: RwLock<(bool, bool, bool)>,
    tool_cache: Mutex<Option<Vec<Value>>>,
    listen: Mutex<Option<(String, Option<oneshot::Sender<()>>)>>,
}

impl Client {
    /// Creates the client and starts dispatching transport events from `events`.
    pub fn new(
        transport: Arc<dyn Transport>,
        mut events: mpsc::UnboundedReceiver<TransportEvent>,
        config: ClientConfig,
    ) -> Arc<Self> {
        let mut capabilities = Map::new();
        if config.elicitation.is_some() {
            capabilities.insert("elicitation".into(), json!({"form": {"applyDefaults": true}, "url": {}}));
        }
        // Legacy era only: some legacy servers refuse to run without roots.
        capabilities.insert("roots".into(), json!({}));
        let client = Arc::new(Self {
            transport,
            config,
            capabilities: Value::Object(capabilities),
            next_id: AtomicI64::new(0),
            pending: Mutex::new(HashMap::new()),
            handlers: Mutex::new(HashMap::new()),
            progress: Mutex::new(Default::default()),
            connected: AtomicBool::new(true),
            server_capabilities: RwLock::new(None),
            server_info: RwLock::new(None),
            instructions: RwLock::new(None),
            negotiated: RwLock::new(None),
            callbacks: RwLock::new(Callbacks::default()),
            list_changed: RwLock::new((false, false, false)),
            tool_cache: Mutex::new(None),
            listen: Mutex::new(None),
        });
        let weak = Arc::downgrade(&client);
        tokio::spawn(async move {
            while let Some(event) = events.recv().await {
                let Some(client) = weak.upgrade() else { break };
                match event {
                    TransportEvent::Message(message) => client.on_message(message),
                    TransportEvent::Error(error) => client.on_error(&error),
                    TransportEvent::Closed => client.on_close(),
                }
            }
        });
        client
    }

    pub fn capabilities(&self) -> &Value {
        &self.capabilities
    }

    pub fn transport(&self) -> &Arc<dyn Transport> {
        &self.transport
    }

    pub fn server_capabilities(&self) -> Option<Value> {
        self.server_capabilities.read().clone()
    }

    pub fn server_info(&self) -> Option<Value> {
        self.server_info.read().clone()
    }

    pub fn instructions(&self) -> Option<String> {
        self.instructions.read().clone()
    }

    pub fn negotiated_protocol_version(&self) -> Option<String> {
        self.negotiated.read().clone()
    }

    /// `getProtocolEra() === "modern"`.
    pub fn is_modern(&self) -> bool {
        self.negotiated.read().as_deref().is_some_and(is_modern)
    }

    pub fn set_on_error(&self, f: impl Fn(&McpError) + Send + Sync + 'static) {
        self.callbacks.write().on_error = Some(Arc::new(f));
    }
    pub fn clear_on_error(&self) {
        self.callbacks.write().on_error = None;
    }
    pub fn set_on_close(&self, f: impl Fn() + Send + Sync + 'static) {
        self.callbacks.write().on_close = Some(Arc::new(f));
    }
    pub fn set_on_tools_changed(&self, f: impl Fn() + Send + Sync + 'static) {
        self.callbacks.write().tools = Some(Arc::new(f));
    }
    pub fn set_on_prompts_changed(&self, f: impl Fn() + Send + Sync + 'static) {
        self.callbacks.write().prompts = Some(Arc::new(f));
    }
    pub fn set_on_resources_changed(&self, f: impl Fn() + Send + Sync + 'static) {
        self.callbacks.write().resources = Some(Arc::new(f));
    }

    fn on_error(&self, error: &McpError) {
        let callback = self.callbacks.read().on_error.clone();
        if let Some(callback) = callback {
            callback(error);
        }
    }

    fn on_close(&self) {
        if !self.connected.swap(false, Ordering::SeqCst) {
            return;
        }
        let pending: Vec<_> = self.pending.lock().drain().collect();
        self.progress.lock().clear();
        let handlers: Vec<_> = self.handlers.lock().drain().collect();
        let callback = self.callbacks.read().on_close.clone();
        if let Some(callback) = callback {
            callback();
        }
        // Dropping the senders fails every pending request with "Connection closed".
        drop(pending);
        for (_, token) in handlers {
            token.cancel();
        }
    }

    fn on_message(self: &Arc<Self>, message: Value) {
        if jsonrpc::is_response(&message) {
            let id = message.get("id").cloned().unwrap_or(Value::Null);
            if let Some((listen_id, _)) = self.listen.lock().as_ref() {
                if id.as_str() == Some(listen_id.as_str()) {
                    // The server ended the subscription.
                    return;
                }
            }
            let key = jsonrpc::id_key(&id);
            if let Some(n) = id.as_i64() {
                self.progress.lock().remove(&n);
            }
            match self.pending.lock().remove(&key) {
                Some(Pending::Response(sender)) => {
                    let _ = sender.send(message);
                }
                None => self.on_error(&McpError::other(format!(
                    "Received a response for an unknown message ID: {}",
                    serde_json::to_string(&message).unwrap_or_default()
                ))),
            }
        } else if jsonrpc::is_request(&message) {
            if self.is_modern() {
                let method = message["method"].as_str().unwrap_or("");
                self.on_error(&McpError::other(format!(
                    "Dropped inbound request '{method}': not servable on this connection's protocol era"
                )));
                return;
            }
            let this = self.clone();
            tokio::spawn(async move { this.on_request(message).await });
        } else if jsonrpc::is_notification(&message) {
            self.on_notification(message);
        }
    }

    fn on_notification(self: &Arc<Self>, message: Value) {
        let method = message["method"].as_str().unwrap_or("").to_string();
        let params = message.get("params").cloned().unwrap_or(Value::Null);
        match method.as_str() {
            "notifications/cancelled" => {
                if let Some(id) = params.get("requestId") {
                    if let Some(token) = self.handlers.lock().get(&jsonrpc::id_key(id)) {
                        token.cancel();
                    }
                }
            }
            "notifications/progress" => {
                let token = params.get("progressToken").and_then(|t| t.as_i64().or_else(|| t.as_str()?.parse().ok()));
                if token.is_none_or(|t| !self.progress.lock().contains(&t)) {
                    self.on_error(&McpError::other(format!(
                        "Received a progress notification for an unknown token: {}",
                        serde_json::to_string(&message).unwrap_or_default()
                    )));
                }
            }
            "notifications/subscriptions/acknowledged" => {
                let id = params.get("_meta").and_then(|m| m.get(SUBSCRIPTION_ID_META_KEY)).and_then(Value::as_str);
                let mut listen = self.listen.lock();
                if let Some((listen_id, ack)) = listen.as_mut() {
                    if Some(listen_id.as_str()) == id {
                        if let Some(ack) = ack.take() {
                            let _ = ack.send(());
                        }
                    }
                }
            }
            "notifications/tools/list_changed" => {
                *self.tool_cache.lock() = None;
                if self.list_changed.read().0 {
                    if let Some(cb) = self.callbacks.read().tools.clone() {
                        cb();
                    }
                }
            }
            "notifications/prompts/list_changed" => {
                if self.list_changed.read().1 {
                    if let Some(cb) = self.callbacks.read().prompts.clone() {
                        cb();
                    }
                }
            }
            "notifications/resources/list_changed" => {
                if self.list_changed.read().2 {
                    if let Some(cb) = self.callbacks.read().resources.clone() {
                        cb();
                    }
                }
            }
            "notifications/elicitation/complete" => {
                if let (Some(handler), Some(id)) =
                    (self.config.elicitation.clone(), params.get("elicitationId").and_then(Value::as_str))
                {
                    let server = self.config.server.clone();
                    let id = id.to_string();
                    tokio::spawn(async move { handler.complete(&server, &id).await });
                }
            }
            _ => {}
        }
    }

    async fn on_request(self: Arc<Self>, message: Value) {
        let id = message["id"].clone();
        let method = message["method"].as_str().unwrap_or("").to_string();
        let params = message.get("params").cloned().unwrap_or(Value::Null);
        let token = CancellationToken::new();
        self.handlers.lock().insert(jsonrpc::id_key(&id), token.clone());
        let result = self.dispatch_request(&method, params, token.clone()).await;
        self.handlers.lock().remove(&jsonrpc::id_key(&id));
        if token.is_cancelled() {
            return;
        }
        let response = match result {
            Some(Ok(result)) => json!({"result": result, "jsonrpc": "2.0", "id": id}),
            Some(Err((code, message))) => json!({"jsonrpc": "2.0", "id": id, "error": {"code": code, "message": message}}),
            None => json!({"jsonrpc": "2.0", "id": id, "error": {"code": codes::METHOD_NOT_FOUND, "message": "Method not found"}}),
        };
        if let Err(error) = self.transport.send(response, SendOptions::default()).await {
            self.on_error(&McpError::other(format!("Failed to send response: {}", error.to_js_string())));
        }
    }

    /// Runs a server→client request locally. `None` = method not found.
    async fn dispatch_request(
        &self,
        method: &str,
        params: Value,
        cancel: CancellationToken,
    ) -> Option<Result<Value, (i64, String)>> {
        match method {
            "ping" => Some(Ok(json!({}))),
            "roots/list" => Some(Ok(json!({"roots": [{"uri": self.config.root_uri}]}))),
            "elicitation/create" => {
                let handler = self.config.elicitation.clone()?;
                Some(self.elicit(handler, params, cancel).await)
            }
            _ => None,
        }
    }

    async fn elicit(
        &self,
        handler: Arc<dyn ElicitationHandler>,
        mut params: Value,
        cancel: CancellationToken,
    ) -> Result<Value, (i64, String)> {
        if !params.is_object() {
            return Err((codes::INVALID_PARAMS, "Invalid elicitation request: expected object".into()));
        }
        if params.get("mode").is_none() {
            params["mode"] = json!("form");
        }
        let mode = params["mode"].as_str().unwrap_or("form").to_string();
        if mode == "form" && !params.get("requestedSchema").is_some_and(Value::is_object) {
            return Err((codes::INVALID_PARAMS, "Invalid elicitation request: requestedSchema is required".into()));
        }
        if mode == "url" && !params.get("url").is_some_and(Value::is_string) {
            return Err((codes::INVALID_PARAMS, "Invalid elicitation request: url is required".into()));
        }
        let mut result = handler
            .create(&self.config.server, params.clone(), cancel)
            .await
            .map_err(|message| (codes::INTERNAL_ERROR, message))?;
        let action = result.get("action").and_then(Value::as_str).unwrap_or("");
        if !matches!(action, "accept" | "decline" | "cancel") {
            return Err((codes::INVALID_PARAMS, format!("Invalid elicitation result: invalid action {action:?}")));
        }
        if mode == "form" && action == "accept" {
            if let Some(content) = result.get_mut("content") {
                apply_elicitation_defaults(&params["requestedSchema"], content);
            }
        }
        Ok(result)
    }

    fn envelope(&self) -> Option<Value> {
        let version = self.negotiated.read().clone()?;
        if !is_modern(&version) {
            return None;
        }
        Some(json!({
            PROTOCOL_VERSION_META_KEY: version,
            CLIENT_INFO_META_KEY: self.config.client_info,
            CLIENT_CAPABILITIES_META_KEY: self.capabilities,
        }))
    }

    fn with_envelope(&self, mut message: Value) -> Value {
        let Some(envelope) = self.envelope() else { return message };
        let mut params = message.get("params").cloned().unwrap_or_else(|| json!({}));
        let mut meta = envelope.as_object().cloned().unwrap_or_default();
        if let Some(existing) = params.get("_meta").and_then(Value::as_object) {
            for (k, v) in existing {
                meta.insert(k.clone(), v.clone());
            }
        }
        params["_meta"] = Value::Object(meta);
        message["params"] = params;
        message
    }

    /// Sends a notification (`Protocol.notification`).
    pub async fn notify(&self, method: &str, params: Option<Value>) -> Result<(), McpError> {
        if !self.connected.load(Ordering::SeqCst) {
            return Err(McpError::sdk(SdkErrorCode::NotConnected, "Not connected"));
        }
        let mut message = json!({"jsonrpc": "2.0", "method": method});
        if let Some(params) = params {
            message["params"] = params;
        }
        self.transport.send(self.with_envelope(message), SendOptions::default()).await
    }

    /// Sends a request and returns its `result` (era-decoded). JSON-RPC errors become
    /// [`McpError::Protocol`].
    pub async fn request(&self, method: &str, params: Option<Value>, options: &RequestOptions) -> Result<Value, McpError> {
        let raw = self.request_raw(method, params.clone(), options).await?;
        if !self.is_modern() {
            return Ok(raw);
        }
        self.decode_modern(method, params, raw, options).await
    }

    async fn decode_modern(
        &self,
        method: &str,
        params: Option<Value>,
        mut raw: Value,
        options: &RequestOptions,
    ) -> Result<Value, McpError> {
        let mut rounds = 0;
        loop {
            let Some(obj) = raw.as_object() else {
                return Err(McpError::sdk(SdkErrorCode::InvalidResult, format!("Invalid result for {method}: not an object")));
            };
            match obj.get("resultType") {
                None => {
                    return Err(McpError::sdk(
                        SdkErrorCode::InvalidResult,
                        format!("Invalid result for {method}: missing required resultType — servers implementing protocol revision 2026-07-28 MUST include it (the absent-means-complete bridge applies only to earlier-revision servers)"),
                    ));
                }
                Some(Value::String(t)) if t == "complete" => {
                    let mut lifted = obj.clone();
                    lifted.remove("resultType");
                    return Ok(Value::Object(lifted));
                }
                Some(Value::String(t)) if t == "input_required" => {
                    let input_requests = obj.get("inputRequests").and_then(Value::as_object).cloned().unwrap_or_default();
                    let request_state = obj.get("requestState").and_then(Value::as_str).map(String::from);
                    if input_requests.is_empty() && request_state.is_none() {
                        return Err(McpError::sdk(
                            SdkErrorCode::InvalidResult,
                            format!("Invalid result for {method}: input_required carries neither inputRequests nor requestState (every input_required result must include at least one of the two)"),
                        ));
                    }
                    rounds += 1;
                    if rounds > INPUT_REQUIRED_MAX_ROUNDS {
                        return Err(McpError::sdk(
                            SdkErrorCode::InputRequiredRoundsExceeded,
                            format!("Multi-round-trip request '{method}' still required input after {INPUT_REQUIRED_MAX_ROUNDS} rounds (inputRequired.maxRounds)"),
                        ));
                    }
                    let mut responses = Map::new();
                    if input_requests.is_empty() {
                        tokio::time::sleep(REQUEST_STATE_ONLY_LEG_PACING).await;
                    }
                    for (key, entry) in input_requests {
                        let Some(inner) = entry.get("method").and_then(Value::as_str) else {
                            return Err(McpError::sdk(
                                SdkErrorCode::InvalidResult,
                                format!("Invalid input request '{key}': each inputRequests entry must be an embedded request object with a method"),
                            ));
                        };
                        if !matches!(inner, "elicitation/create" | "sampling/createMessage" | "roots/list") {
                            return Err(McpError::sdk(
                                SdkErrorCode::InvalidResult,
                                format!("Invalid input request '{key}': '{inner}' is not an embedded request the 2026-07-28 revision defines (expected elicitation/create, sampling/createMessage, or roots/list)"),
                            ));
                        }
                        let inner_params = entry.get("params").cloned().unwrap_or(Value::Null);
                        let cancel = options.cancel.clone().unwrap_or_default();
                        match self.dispatch_request(inner, inner_params, cancel).await {
                            None => {
                                return Err(McpError::sdk(
                                    SdkErrorCode::CapabilityNotSupported,
                                    format!("Cannot fulfil input request '{key}': no handler is registered for '{inner}' on this client. Declare the corresponding capability and register a handler, or handle input_required results manually."),
                                ));
                            }
                            Some(Ok(value)) => {
                                responses.insert(key, value);
                            }
                            Some(Err((code, message))) => return Err(McpError::Protocol { code, message, data: None }),
                        }
                    }
                    let mut retry = params.clone().unwrap_or_else(|| json!({}));
                    if !responses.is_empty() {
                        retry["inputResponses"] = Value::Object(responses);
                    }
                    if let Some(state) = request_state {
                        retry["requestState"] = json!(state);
                    }
                    raw = self.request_raw(method, Some(retry), options).await?;
                }
                Some(Value::String(other)) => {
                    return Err(McpError::sdk(
                        SdkErrorCode::UnsupportedResultType,
                        format!("Unsupported result type '{other}' for {method}"),
                    ));
                }
                Some(_) => {
                    return Err(McpError::sdk(
                        SdkErrorCode::InvalidResult,
                        format!("Invalid result for {method}: non-string resultType"),
                    ));
                }
            }
        }
    }

    async fn request_raw(&self, method: &str, params: Option<Value>, options: &RequestOptions) -> Result<Value, McpError> {
        if !self.connected.load(Ordering::SeqCst) {
            return Err(McpError::other("Not connected"));
        }
        if let Some(cancel) = &options.cancel {
            if cancel.is_cancelled() {
                return Err(McpError::sdk(SdkErrorCode::RequestTimeout, "AbortError: This operation was aborted"));
            }
        }
        let id = self.next_id.fetch_add(1, Ordering::SeqCst);
        let mut message = json!({"method": method});
        if let Some(params) = params {
            message["params"] = params;
        }
        message["jsonrpc"] = json!("2.0");
        message["id"] = json!(id);
        if options.progress {
            self.progress.lock().insert(id);
            let mut params = message.get("params").cloned().unwrap_or_else(|| json!({}));
            let mut meta = params.get("_meta").and_then(Value::as_object).cloned().unwrap_or_default();
            meta.insert("progressToken".into(), json!(id));
            params["_meta"] = Value::Object(meta);
            message["params"] = params;
        }
        let message = self.with_envelope(message);
        let key = jsonrpc::id_key(&json!(id));
        let (tx, rx) = oneshot::channel();
        self.pending.lock().insert(key.clone(), Pending::Response(tx));
        let per_request = self.is_modern() && self.transport.has_per_request_stream();
        let request_cancel = per_request.then(CancellationToken::new);
        let send_options = SendOptions { request_cancel: request_cancel.clone(), ..Default::default() };
        let transport = self.transport.clone();
        let (send_err_tx, send_err_rx) = oneshot::channel::<McpError>();
        tokio::spawn(async move {
            if let Err(error) = transport.send(message, send_options).await {
                let _ = send_err_tx.send(error);
            }
        });
        let timeout = options.timeout.unwrap_or(DEFAULT_REQUEST_TIMEOUT);
        let cancel = options.cancel.clone().unwrap_or_default();
        let outcome = tokio::select! {
            response = rx => match response {
                Ok(response) => Ok(response),
                Err(_) => Err(McpError::connection_closed()),
            },
            Ok(error) = send_err_rx => Err(error),
            _ = tokio::time::sleep(timeout) => {
                self.cancel_request(id, &McpError::request_timeout().to_js_string(), request_cancel.as_ref()).await;
                Err(McpError::request_timeout())
            }
            _ = cancel.cancelled() => {
                self.cancel_request(id, "AbortError: This operation was aborted", request_cancel.as_ref()).await;
                Err(McpError::sdk(SdkErrorCode::RequestTimeout, "AbortError: This operation was aborted"))
            }
        };
        self.pending.lock().remove(&key);
        self.progress.lock().remove(&id);
        let response = outcome?;
        if let Some(error) = response.get("error") {
            return Err(McpError::from_rpc_error(
                error.get("code").and_then(Value::as_i64).unwrap_or(codes::INTERNAL_ERROR),
                error.get("message").and_then(Value::as_str).unwrap_or("").to_string(),
                error.get("data").cloned(),
            ));
        }
        Ok(response.get("result").cloned().unwrap_or(Value::Null))
    }

    async fn cancel_request(&self, id: i64, reason: &str, request_cancel: Option<&CancellationToken>) {
        self.progress.lock().remove(&id);
        match request_cancel {
            Some(token) => token.cancel(),
            None => {
                let notification = self.with_envelope(json!({
                    "jsonrpc": "2.0",
                    "method": "notifications/cancelled",
                    "params": {"requestId": id, "reason": reason}
                }));
                if let Err(error) = self.transport.send(notification, SendOptions::default()).await {
                    self.on_error(&McpError::other(format!("Failed to send cancellation: {}", error.to_js_string())));
                }
            }
        }
    }

    /// `Client.connect`: legacy handshake, or era negotiation for `auto` / a pinned revision.
    pub async fn connect(&self, timeout: Duration) -> Result<(), McpError> {
        match self.config.negotiation {
            None | Some(ProtocolSetting::Legacy) => {
                self.transport.start().await?;
                self.legacy_handshake(timeout).await
            }
            Some(setting) => {
                let result = self.negotiate(setting, timeout).await;
                match result {
                    Err(error) => {
                        self.transport.close().await;
                        Err(error)
                    }
                    Ok(None) => self.legacy_handshake(timeout).await,
                    Ok(Some((version, discover))) => {
                        self.apply_modern(version, discover, timeout).await;
                        Ok(())
                    }
                }
            }
        }
    }

    async fn legacy_handshake(&self, timeout: Duration) -> Result<(), McpError> {
        let result = async {
            let options = RequestOptions { timeout: Some(timeout), ..Default::default() };
            let result = self
                .request(
                    "initialize",
                    Some(json!({
                        "protocolVersion": LATEST_PROTOCOL_VERSION,
                        "capabilities": self.capabilities,
                        "clientInfo": self.config.client_info,
                    })),
                    &options,
                )
                .await?;
            let version = result.get("protocolVersion").and_then(Value::as_str).map(String::from);
            let Some(version) = version.filter(|v| SUPPORTED_PROTOCOL_VERSIONS.contains(&v.as_str())) else {
                return Err(McpError::other(format!(
                    "Server's protocol version is not supported: {}",
                    result.get("protocolVersion").map(|v| v.as_str().map(String::from).unwrap_or(v.to_string())).unwrap_or("undefined".into())
                )));
            };
            *self.server_capabilities.write() = result.get("capabilities").cloned();
            *self.server_info.write() = result.get("serverInfo").cloned();
            self.transport.set_protocol_version(&version);
            *self.instructions.write() = result.get("instructions").and_then(Value::as_str).map(String::from);
            self.notify("notifications/initialized", None).await?;
            *self.negotiated.write() = Some(version);
            self.setup_list_changed();
            Ok(())
        }
        .await;
        if result.is_err() {
            self.transport.close().await;
        }
        result
    }

    fn setup_list_changed(&self) {
        let caps = self.server_capabilities.read().clone().unwrap_or(Value::Null);
        let flag = |key: &str| caps.get(key).and_then(|c| c.get("listChanged")).and_then(Value::as_bool) == Some(true);
        *self.list_changed.write() = (flag("tools"), flag("prompts"), flag("resources"));
    }

    /// Era negotiation: `Ok(None)` = legacy, `Ok(Some(version, discover))` = modern.
    async fn negotiate(&self, setting: ProtocolSetting, timeout: Duration) -> Result<Option<(String, Value)>, McpError> {
        self.transport.start().await?;
        let pin = setting == ProtocolSetting::V2026_07_28;
        let fallback_available = !pin;
        let modern_versions = [MODERN_PROTOCOL_VERSION.to_string()];
        let mut requested = modern_versions[0].clone();
        let mut corrective_used = false;
        let mut probe_counter = 0;
        loop {
            probe_counter += 1;
            let id = format!("server-discover-probe-{probe_counter}");
            let probe = json!({
                "jsonrpc": "2.0",
                "id": id,
                "method": "server/discover",
                "params": {"_meta": {
                    PROTOCOL_VERSION_META_KEY: requested,
                    CLIENT_INFO_META_KEY: self.config.client_info,
                    CLIENT_CAPABILITIES_META_KEY: self.capabilities,
                }}
            });
            let key = jsonrpc::id_key(&json!(id));
            let (tx, rx) = oneshot::channel();
            self.pending.lock().insert(key.clone(), Pending::Response(tx));
            let transport = self.transport.clone();
            let (err_tx, err_rx) = oneshot::channel::<McpError>();
            tokio::spawn(async move {
                if let Err(error) = transport.send(probe, SendOptions::default()).await {
                    let _ = err_tx.send(error);
                }
            });
            enum Outcome {
                Result(Value),
                Rpc(i64, String, Option<Value>),
                Http(u16, Option<String>, String),
                Auth(McpError),
                Network(McpError),
                Closed,
                Timeout,
            }
            let outcome = tokio::select! {
                response = rx => match response {
                    Ok(response) => match response.get("error") {
                        Some(error) => Outcome::Rpc(
                            error.get("code").and_then(Value::as_i64).unwrap_or(0),
                            error.get("message").and_then(Value::as_str).unwrap_or("").to_string(),
                            error.get("data").cloned(),
                        ),
                        None => Outcome::Result(response.get("result").cloned().unwrap_or(Value::Null)),
                    },
                    Err(_) => Outcome::Closed,
                },
                Ok(error) = err_rx => match error {
                    McpError::Unauthorized(_) => Outcome::Auth(error),
                    McpError::Http { status, text, status_text, .. } => Outcome::Http(status, text, status_text),
                    other => Outcome::Network(other),
                },
                _ = tokio::time::sleep(timeout) => Outcome::Timeout,
            };
            self.pending.lock().remove(&key);
            // Upstream's custom stdio transport is classified as "http" by the SDK (no stderr/pid).
            let verdict: Result<Verdict, McpError> = match outcome {
                Outcome::Result(result) => Ok(classify_result(&result, &modern_versions, &requested, fallback_available)?),
                Outcome::Rpc(code, message, data) => classify_rpc(code, message, data, &modern_versions, &requested, fallback_available),
                Outcome::Http(status, body, status_text) => {
                    if status == 401 || status == 403 {
                        let denial = status == 403;
                        Err(McpError::Http {
                            code: if denial { SdkErrorCode::ClientHttpForbidden } else { SdkErrorCode::ClientHttpAuthentication },
                            message: format!(
                                "Version negotiation failed: {}",
                                if denial { "the server denied access (HTTP 403)" } else { "the server requires authorization (HTTP 401)" }
                            ),
                            status,
                            status_text,
                            text: body,
                        })
                    } else if status >= 500 {
                        Err(McpError::Http {
                            code: SdkErrorCode::EraNegotiationFailed,
                            message: format!("Version negotiation failed: the server answered the probe with HTTP {status}"),
                            status,
                            status_text,
                            text: body,
                        })
                    } else {
                        match body.as_deref().and_then(|b| serde_json::from_str::<Value>(b).ok()).and_then(|v| v.get("error").cloned()) {
                            Some(error) if error.get("code").is_some_and(Value::is_number) => classify_rpc(
                                error["code"].as_i64().unwrap_or(0),
                                error.get("message").and_then(Value::as_str).unwrap_or("").to_string(),
                                error.get("data").cloned(),
                                &modern_versions,
                                &requested,
                                fallback_available,
                            ),
                            _ => Ok(Verdict::Legacy),
                        }
                    }
                }
                Outcome::Auth(error) => Err(error),
                Outcome::Network(error) => Err(McpError::sdk(
                    SdkErrorCode::EraNegotiationFailed,
                    format!("Version negotiation probe failed: {error}"),
                )),
                Outcome::Closed => Err(McpError::sdk(
                    SdkErrorCode::EraNegotiationFailed,
                    "Version negotiation probe failed: Connection closed during the version negotiation probe",
                )),
                Outcome::Timeout => Err(McpError::sdk(
                    SdkErrorCode::RequestTimeout,
                    format!("Version negotiation probe timed out after {}ms", timeout.as_millis()),
                )),
            };
            match verdict? {
                Verdict::Modern(version, discover) => return Ok(Some((version, discover))),
                Verdict::Corrective(version, error) => {
                    if corrective_used {
                        return Err(error);
                    }
                    corrective_used = true;
                    requested = version;
                }
                Verdict::Legacy => {
                    if pin {
                        return Err(McpError::sdk(
                            SdkErrorCode::EraNegotiationFailed,
                            format!("Version negotiation failed: the server did not offer pinned protocol version {MODERN_PROTOCOL_VERSION} via server/discover (no fallback in pin mode)"),
                        ));
                    }
                    return Ok(None);
                }
            }
        }
    }

    async fn apply_modern(&self, version: String, discover: Value, timeout: Duration) {
        *self.server_capabilities.write() = discover.get("capabilities").cloned();
        *self.server_info.write() = discover.get("_meta").and_then(|m| m.get(SERVER_INFO_META_KEY)).cloned();
        *self.instructions.write() = discover.get("instructions").and_then(Value::as_str).map(String::from);
        *self.negotiated.write() = Some(version.clone());
        self.transport.set_protocol_version(&version);
        self.setup_list_changed();
        let (tools, prompts, resources) = *self.list_changed.read();
        let mut filter = Map::new();
        if tools {
            filter.insert("toolsListChanged".into(), json!(true));
        }
        if prompts {
            filter.insert("promptsListChanged".into(), json!(true));
        }
        if resources {
            filter.insert("resourcesListChanged".into(), json!(true));
        }
        if filter.is_empty() {
            return;
        }
        let listen_id = "listen:0".to_string();
        let (ack_tx, ack_rx) = oneshot::channel();
        *self.listen.lock() = Some((listen_id.clone(), Some(ack_tx)));
        let request = json!({
            "jsonrpc": "2.0",
            "id": listen_id,
            "method": "subscriptions/listen",
            "params": {"_meta": self.envelope().unwrap_or(json!({})), "notifications": filter}
        });
        let transport = self.transport.clone();
        let send_options = SendOptions { request_cancel: Some(CancellationToken::new()), ..Default::default() };
        let (err_tx, err_rx) = oneshot::channel::<McpError>();
        tokio::spawn(async move {
            if let Err(error) = transport.send(request, send_options).await {
                let _ = err_tx.send(error);
            }
        });
        let outcome = tokio::select! {
            _ = ack_rx => None,
            Ok(error) = err_rx => Some(error),
            _ = tokio::time::sleep(timeout) => Some(McpError::sdk(SdkErrorCode::RequestTimeout, "subscriptions/listen ack timed out")),
        };
        if let Some(error) = outcome {
            self.on_error(&error);
        }
    }

    /// `_listAllPages`: follows `nextCursor` (stopping on a repeated cursor) up to 64 pages.
    pub async fn list_all(&self, method: &str, key: &str, options: &RequestOptions) -> Result<Vec<Value>, McpError> {
        let first = self.request(method, None, options).await?;
        let mut items: Vec<Value> = first.get(key).and_then(Value::as_array).cloned().ok_or_else(|| {
            McpError::sdk(SdkErrorCode::InvalidResult, format!("Invalid result for {method}: {key} must be an array"))
        })?;
        let mut cursor = first.get("nextCursor").and_then(Value::as_str).map(String::from);
        let mut seen = std::collections::HashSet::new();
        let mut pages = 1;
        while let Some(c) = cursor.clone() {
            if seen.contains(&c) {
                break;
            }
            if pages >= DEFAULT_LIST_MAX_PAGES {
                return Err(McpError::sdk(
                    SdkErrorCode::ListPaginationExceeded,
                    format!("{method}: exceeded listMaxPages ({DEFAULT_LIST_MAX_PAGES}); server pagination did not terminate"),
                ));
            }
            seen.insert(c.clone());
            let page = self.request(method, Some(json!({"cursor": c})), options).await?;
            if let Some(more) = page.get(key).and_then(Value::as_array) {
                items.extend(more.iter().cloned());
            }
            cursor = page.get("nextCursor").and_then(Value::as_str).map(String::from);
            pages += 1;
        }
        Ok(items)
    }

    fn has_capability(&self, key: &str) -> bool {
        self.server_capabilities.read().as_ref().and_then(|c| c.get(key)).is_some_and(|v| !v.is_null())
    }

    pub async fn list_tools(&self, options: &RequestOptions) -> Result<Vec<Value>, McpError> {
        if !self.has_capability("tools") {
            return Ok(vec![]);
        }
        let tools = self.list_all("tools/list", "tools", options).await?;
        *self.tool_cache.lock() = Some(tools.clone());
        Ok(tools)
    }

    pub async fn list_prompts(&self, options: &RequestOptions) -> Result<Vec<Value>, McpError> {
        if !self.has_capability("prompts") {
            return Ok(vec![]);
        }
        self.list_all("prompts/list", "prompts", options).await
    }

    pub async fn list_resources(&self, options: &RequestOptions) -> Result<Vec<Value>, McpError> {
        if !self.has_capability("resources") {
            return Ok(vec![]);
        }
        self.list_all("resources/list", "resources", options).await
    }

    pub async fn list_resource_templates(&self, options: &RequestOptions) -> Result<Vec<Value>, McpError> {
        if !self.has_capability("resources") {
            return Ok(vec![]);
        }
        self.list_all("resources/templates/list", "resourceTemplates", options).await
    }

    pub fn has_resources(&self) -> bool {
        self.has_capability("resources")
    }

    /// `Client.callTool`, including output-schema validation against the last `tools/list`.
    pub async fn call_tool(&self, params: Value, options: &RequestOptions) -> Result<Value, McpError> {
        let name = params.get("name").and_then(Value::as_str).unwrap_or("").to_string();
        let schema = self
            .tool_cache
            .lock()
            .as_ref()
            .and_then(|tools| tools.iter().find(|t| t.get("name").and_then(Value::as_str) == Some(name.as_str())).cloned())
            .and_then(|t| t.get("outputSchema").cloned());
        let validator = match &schema {
            Some(schema) => match jsonschema::validator_for(schema) {
                Ok(v) => Some(v),
                Err(error) => {
                    let message: String = error.to_string().chars().take(200).collect();
                    return Err(McpError::Protocol {
                        code: codes::INVALID_PARAMS,
                        message: format!("Tool '{name}' has an invalid outputSchema: {message}"),
                        data: None,
                    });
                }
            },
            None => None,
        };
        let result = self.request("tools/call", Some(params), options).await?;
        if let Some(validator) = validator {
            let is_error = result.get("isError") == Some(&Value::Bool(true));
            match result.get("structuredContent") {
                None if !is_error => {
                    return Err(McpError::Protocol {
                        code: codes::INVALID_REQUEST,
                        message: format!("Tool {name} has an output schema but did not return structured content"),
                        data: None,
                    });
                }
                Some(structured) if !is_error => {
                    let errors: Vec<String> = validator.iter_errors(structured).map(|e| e.to_string()).collect();
                    if !errors.is_empty() {
                        return Err(McpError::Protocol {
                            code: codes::INVALID_PARAMS,
                            message: format!("Structured content does not match the tool's output schema: {}", errors.join(", ")),
                            data: None,
                        });
                    }
                }
                _ => {}
            }
        }
        Ok(result)
    }

    /// `Client.close()`.
    pub async fn close(&self) {
        self.transport.close().await;
    }
}

enum Verdict {
    Modern(String, Value),
    Corrective(String, McpError),
    Legacy,
}

fn classify_result(result: &Value, modern: &[String], requested: &str, fallback: bool) -> Result<Verdict, McpError> {
    let supported = result.get("supportedVersions").and_then(Value::as_array);
    let valid = supported.is_some_and(|s| s.iter().all(Value::is_string))
        && result.get("capabilities").is_some_and(Value::is_object)
        && result.get("instructions").is_none_or(Value::is_string);
    if !valid {
        return Ok(Verdict::Legacy);
    }
    let supported: Vec<String> = supported.unwrap().iter().filter_map(|v| v.as_str().map(String::from)).collect();
    if let Some(version) = modern.iter().find(|v| supported.contains(v)) {
        let mut discover = result.clone();
        if let Some(obj) = discover.as_object_mut() {
            obj.remove("resultType");
        }
        return Ok(Verdict::Modern(version.clone(), discover));
    }
    if fallback {
        return Ok(Verdict::Legacy);
    }
    Err(McpError::UnsupportedProtocolVersion {
        message: format!("Unsupported protocol version: {requested}"),
        supported,
        requested: requested.to_string(),
    })
}

fn classify_rpc(
    code: i64,
    message: String,
    data: Option<Value>,
    modern: &[String],
    requested: &str,
    fallback: bool,
) -> Result<Verdict, McpError> {
    if code != codes::UNSUPPORTED_PROTOCOL_VERSION {
        return Ok(Verdict::Legacy);
    }
    let supported: Option<Vec<String>> = data
        .as_ref()
        .and_then(|d| d.get("supported"))
        .and_then(Value::as_array)
        .filter(|a| !a.is_empty() && a.iter().all(Value::is_string))
        .map(|a| a.iter().filter_map(|v| v.as_str().map(String::from)).collect());
    let Some(supported) = supported else { return Ok(Verdict::Legacy) };
    let req = data
        .as_ref()
        .and_then(|d| d.get("requested"))
        .and_then(Value::as_str)
        .unwrap_or(requested)
        .to_string();
    let error = McpError::UnsupportedProtocolVersion { message, supported: supported.clone(), requested: req };
    let supported_modern: Vec<&String> = supported.iter().filter(|v| is_modern(v)).collect();
    if let Some(mutual) = modern.iter().find(|v| supported_modern.contains(v)) {
        return Ok(Verdict::Corrective(mutual.clone(), error));
    }
    if !supported_modern.is_empty() {
        return Err(error);
    }
    if fallback { Ok(Verdict::Legacy) } else { Err(error) }
}

/// `applyElicitationDefaults`.
pub fn apply_elicitation_defaults(schema: &Value, data: &mut Value) {
    if !data.is_object() {
        return;
    }
    if schema.get("type").and_then(Value::as_str) == Some("object") {
        if let Some(props) = schema.get("properties").and_then(Value::as_object) {
            for (key, prop) in props {
                let obj = data.as_object_mut().unwrap();
                if !obj.contains_key(key) {
                    if let Some(default) = prop.get("default") {
                        obj.insert(key.clone(), default.clone());
                    }
                }
                if let Some(value) = obj.get_mut(key) {
                    apply_elicitation_defaults(prop, value);
                }
            }
        }
    }
    for key in ["anyOf", "oneOf"] {
        if let Some(subs) = schema.get(key).and_then(Value::as_array) {
            for sub in subs {
                if !sub.is_boolean() {
                    apply_elicitation_defaults(sub, data);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn elicitation_defaults() {
        let schema = json!({"type":"object","properties":{"a":{"type":"string","default":"x"},"b":{"type":"number"}}});
        let mut data = json!({"b": 1});
        apply_elicitation_defaults(&schema, &mut data);
        assert_eq!(data, json!({"b": 1, "a": "x"}));
    }

    #[test]
    fn probe_classification() {
        let modern = vec![MODERN_PROTOCOL_VERSION.to_string()];
        assert!(matches!(
            classify_result(&json!({"supportedVersions":["2026-07-28"],"capabilities":{}}), &modern, "2026-07-28", true).unwrap(),
            Verdict::Modern(..)
        ));
        assert!(matches!(classify_result(&json!({}), &modern, "2026-07-28", true).unwrap(), Verdict::Legacy));
        assert!(classify_result(&json!({"supportedVersions":["2027-01-01"],"capabilities":{}}), &modern, "2026-07-28", false).is_err());
        assert!(matches!(classify_rpc(-32601, "x".into(), None, &modern, "2026-07-28", true).unwrap(), Verdict::Legacy));
        assert!(matches!(
            classify_rpc(-32022, "x".into(), Some(json!({"supported":["2026-07-28"]})), &modern, "2027", true).unwrap(),
            Verdict::Corrective(..)
        ));
    }
}
