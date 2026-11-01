//! The built-in `opencode.browser` plugin (`packages/plugin-browser`, 2.2): the desktop app's browser
//! attaches to a session over the `experimental.browser` RPC, and the `browser.*` Code Mode tools drive
//! it through a command/result round trip.
//!
//! - `connection.ts` → [`Browsers`]: one attachment per session and location. `attach` stays pending
//!   for the attachment's lifetime and answers `closed` or `replaced`; while it is attached the session
//!   carries `allow` rules for the browser tools and the `browser` skill, which every agent denies
//!   (`location::browser_denies`). `state` stores the desktop's tab inventory, `command` hands out a
//!   pending request, `result` completes it. Control events (`rpc.experimental.browser.control`) announce
//!   the attachment, each request ID and cancellations; arguments and results never go on the event feed.
//! - `tools.ts` → [`tools`]: the 45 operations of `Browser.Operations` as Code Mode tools, with the
//!   definitions upstream's tool runtime exports (`assets/browser/schemas.json`, regenerated with
//!   `scripts/browser-schemas.ts`), URL normalization, result validation and capture export.
//! - `files.ts` → [`files`], `tunnel.ts` → [`tunnel`] (TCP relays for the desktop's proxy).
//!
//! `proxy.ts` is the desktop's own HTTP/CONNECT listener in front of the tunnels; the server never loads it.

pub mod files;
pub mod schema;
pub mod tools;
pub mod tunnel;

use std::collections::HashMap;
use std::sync::{Arc, LazyLock};
use std::time::Duration;

use base64::Engine;
use oc_schema::types::{Effect, LocationRef, PermissionRule};
use parking_lot::Mutex;
use serde_json::{Map, Value, json};
use tokio::sync::{oneshot, watch};
use tokio_util::sync::CancellationToken;

use crate::bus::Bus;
use crate::session::Sessions;

pub const PLUGIN_ID: &str = "opencode.browser";
pub const RPC_ID: &str = "experimental.browser";
/// The control event's bus type (`rpc.<id>.<event>`).
pub const CONTROL: &str = "rpc.experimental.browser.control";
pub const MAX_FILE_BYTES: usize = 5 * 1024 * 1024;
pub const TUNNEL_CHUNK_BYTES: usize = 64 * 1024;
const VERSION: u64 = 4;
/// How long a browser request waits for the desktop's result.
pub const REQUEST_TIMEOUT: Duration = Duration::from_secs(60);

/// The generated schemas: every operation's definition and every RPC method's input.
pub(crate) static SCHEMAS: LazyLock<Value> =
    LazyLock::new(|| serde_json::from_str(include_str!("../../../../assets/browser/schemas.json")).expect("browser schemas parse"));

/// The JSON Schema document (definitions under `$defs`) of an RPC method's input, for the server's
/// Effect-compatible input check (`oc_server::validate::schema_issue`).
pub fn input_schema(method: &str) -> Option<&'static Value> {
    SCHEMAS["rpc"].get(method)
}

/// A failed RPC call: the failure `type`, its message and the declared error's data.
#[derive(Debug, Clone, PartialEq)]
pub struct RpcFailure {
    pub kind: String,
    pub message: String,
    pub data: Option<Value>,
}

impl RpcFailure {
    fn system(kind: &str, message: impl Into<String>) -> Self {
        RpcFailure { kind: kind.into(), message: message.into(), data: None }
    }

    /// `call.error("unavailable", message, {})`.
    fn unavailable(message: impl Into<String>) -> Self {
        RpcFailure { kind: "unavailable".into(), message: message.into(), data: Some(json!({})) }
    }

    fn rpc_unavailable() -> Self {
        Self::system("rpc.unavailable", format!("RPC is unavailable: {RPC_ID}"))
    }
}

/// How an attachment ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum End {
    Closed,
    Replaced,
    /// The location's RPC service closed under the pending `attach` (`rpc.unavailable`).
    Unavailable,
}

/// `Browser.Result`: the desktop's JSON value and its files.
#[derive(Debug, Clone, PartialEq)]
pub struct BrowserResult {
    pub value: Value,
    pub files: Vec<files::File>,
}

struct Pending {
    /// The encoded `Browser.Command` the desktop retrieves.
    command: Value,
    result: Option<oneshot::Sender<Result<BrowserResult, String>>>,
}

/// One desktop attachment of a session.
pub(crate) struct Attachment {
    connection_id: String,
    state: Mutex<Value>,
    end: watch::Sender<Option<End>>,
    pending: Mutex<HashMap<String, Pending>>,
    tunnels: tunnel::Tunnels,
    /// Whether its `allow` rules are on the session and still to be removed.
    granted: Mutex<bool>,
}

impl Attachment {
    fn new(connection_id: &str) -> Self {
        Attachment {
            connection_id: connection_id.to_string(),
            state: Mutex::new(json!({ "tabs": [], "focusedTabID": null })),
            end: watch::channel(None).0,
            pending: Mutex::new(HashMap::new()),
            tunnels: tunnel::Tunnels::new(),
            granted: Mutex::new(false),
        }
    }

    fn finish(&self, end: End) {
        self.tunnels.dispose();
        self.end.send_if_modified(|current| {
            if current.is_some() {
                return false;
            }
            *current = Some(end);
            true
        });
    }

    async fn ended(&self) -> End {
        let mut rx = self.end.subscribe();
        loop {
            if let Some(end) = *rx.borrow_and_update() {
                return end;
            }
            if rx.changed().await.is_err() {
                return End::Closed;
            }
        }
    }
}

/// The plugin instance of one location (`BrowserConnection.make`).
struct Instance {
    key: String,
    location: LocationRef,
    /// `active`: false once the instance is finalized (no new attachment).
    active: Mutex<bool>,
    /// The location's RPC service closed (`Rpc.close`): every call fails `rpc.unavailable`.
    closed: CancellationToken,
    browsers: Mutex<HashMap<String, Arc<Attachment>>>,
}

impl Instance {
    /// `close(sessionID, reason)`.
    fn close(&self, session_id: &str, end: End) {
        let removed = self.browsers.lock().remove(session_id);
        if let Some(browser) = removed {
            browser.finish(end);
        }
    }
}

/// The browser attachments of every location.
pub struct Browsers {
    bus: Bus,
    sessions: Sessions,
    instances: Mutex<HashMap<String, Arc<Instance>>>,
    timeout: Mutex<Duration>,
    watching: std::sync::Once,
}

impl Browsers {
    pub fn new(bus: Bus, version: &str) -> Arc<Browsers> {
        Arc::new(Browsers {
            sessions: Sessions::new(bus.clone(), version.to_string()),
            bus,
            instances: Mutex::new(HashMap::new()),
            timeout: Mutex::new(REQUEST_TIMEOUT),
            watching: std::sync::Once::new(),
        })
    }

    /// Overrides the 60-second request timeout (tests).
    pub fn set_timeout(&self, timeout: Duration) {
        *self.timeout.lock() = timeout;
    }

    /// `session.deleted` / `session.moved` end the session's attachment.
    fn watch_sessions(self: &Arc<Self>) {
        let Ok(handle) = tokio::runtime::Handle::try_current() else { return };
        let mut rx = self.bus.subscribe();
        let me = Arc::downgrade(self);
        handle.spawn(async move {
            loop {
                let event = match rx.recv().await {
                    Ok(event) => event,
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                    Err(_) => return,
                };
                if event.kind != "session.deleted" && event.kind != "session.moved" {
                    continue;
                }
                let Some(me) = me.upgrade() else { return };
                let Some(session) = event.data.get("sessionID").and_then(Value::as_str) else { continue };
                let instances: Vec<Arc<Instance>> = me.instances.lock().values().cloned().collect();
                for instance in instances {
                    instance.close(session, End::Closed);
                }
            }
        });
    }

    fn instance(self: &Arc<Self>, key: &str, location: &LocationRef) -> Arc<Instance> {
        self.watching.call_once(|| self.watch_sessions());
        self.instances
            .lock()
            .entry(key.to_string())
            .or_insert_with(|| {
                Arc::new(Instance {
                    key: key.to_string(),
                    location: location.clone(),
                    active: Mutex::new(true),
                    closed: CancellationToken::new(),
                    browsers: Mutex::new(HashMap::new()),
                })
            })
            .clone()
    }

    /// The location's RPC service closed (eviction, reload, shutdown): pending calls fail with
    /// `rpc.unavailable` and its attachments end; a later call starts a fresh instance.
    pub fn close_location(&self, key: &str) {
        let Some(instance) = self.instances.lock().remove(key) else { return };
        *instance.active.lock() = false;
        instance.closed.cancel();
        let browsers: Vec<(String, Arc<Attachment>)> = instance.browsers.lock().drain().collect();
        for (_, browser) in browsers {
            browser.finish(End::Unavailable);
        }
    }

    /// The plugin restarted or was disabled at the location (its activation scope closed): every
    /// attachment ends with `closed`, and the next call reaches a fresh instance.
    pub fn restart(&self, key: &str) {
        let Some(instance) = self.instances.lock().remove(key) else { return };
        *instance.active.lock() = false;
        let browsers: Vec<(String, Arc<Attachment>)> = instance.browsers.lock().drain().collect();
        for (_, browser) in browsers {
            browser.finish(End::Closed);
        }
    }

    /// Whether a session at the location has a desktop attached.
    pub fn attached(&self, key: &str, session_id: &str) -> bool {
        self.instances.lock().get(key).is_some_and(|i| i.browsers.lock().contains_key(session_id))
    }

    fn publish(&self, instance: &Instance, data: Value) {
        self.bus.publish(CONTROL, data, Some(instance.location.clone()));
    }

    /// `Rpc.call("experimental.browser", method, input)` at a location (`key` is its canonical
    /// directory). Answers `Value::Null` for `void` outputs. `check` validates the input and returns the
    /// first issue (the HTTP route passes the server's Effect-compatible checker); without one the input is
    /// checked with the tool validator, whose messages differ for some unions.
    pub async fn call(
        self: &Arc<Self>,
        key: &str,
        location: &LocationRef,
        method: &str,
        input: Value,
        check: Option<&(dyn Fn(&Value, &Value) -> Option<String> + Sync)>,
    ) -> Result<Value, RpcFailure> {
        let Some(schema) = input_schema(method) else {
            return Err(RpcFailure::system("rpc.method_not_found", format!("Unknown RPC method: {RPC_ID}.{method}")));
        };
        let issue = match check {
            Some(check) => check(schema, &input),
            None => input_issue(schema, &input),
        };
        if let Some(message) = issue {
            return Err(RpcFailure::system("rpc.invalid_input", message));
        }
        let input = schema::js_numbers(input);
        let instance = self.instance(key, location);
        let closed = instance.closed.clone();
        let handler = self.handle(&instance, method, &input);
        tokio::select! {
            result = handler => result,
            _ = closed.cancelled() => Err(RpcFailure::rpc_unavailable()),
        }
    }

    async fn handle(self: &Arc<Self>, instance: &Arc<Instance>, method: &str, input: &Value) -> Result<Value, RpcFailure> {
        let session = input["sessionID"].as_str().unwrap_or_default().to_string();
        let connection = input["connectionID"].as_str().unwrap_or_default().to_string();
        match method {
            "attach" => self.attach(instance, &session, &connection).await,
            "state" => {
                let browser = current(instance, &session, &connection).ok_or_else(|| RpcFailure::unavailable("Browser is unavailable."))?;
                let state = schema::decode(&SCHEMAS["rpc"]["state"]["$defs"]["Browser.State"], &input["state"])
                    .unwrap_or(json!({ "tabs": [], "focusedTabID": null }));
                *browser.state.lock() = state;
                Ok(Value::Null)
            }
            "command" => {
                let request = input["requestID"].as_str().unwrap_or_default();
                let command =
                    current(instance, &session, &connection).and_then(|b| b.pending.lock().get(request).map(|p| p.command.clone()));
                command.ok_or_else(|| RpcFailure::unavailable("Browser request is no longer available."))
            }
            "result" => {
                let browser = current(instance, &session, &connection).ok_or_else(|| RpcFailure::unavailable("Browser is unavailable."))?;
                let request = input["requestID"].as_str().unwrap_or_default();
                let outcome = &input["outcome"];
                let result = if outcome["type"] == "failure" {
                    Err(format!(
                        "[browser.{}] {}",
                        outcome["code"].as_str().unwrap_or_default(),
                        outcome["message"].as_str().unwrap_or_default()
                    ))
                } else {
                    Ok(decode_result(&outcome["result"]).map_err(|m| RpcFailure::system("rpc.invalid_input", m))?)
                };
                let sender = browser.pending.lock().get_mut(request).and_then(|p| p.result.take());
                if let Some(sender) = sender {
                    let _ = sender.send(result);
                }
                Ok(Value::Null)
            }
            "tunnel.open" | "tunnel.read" | "tunnel.write" | "tunnel.close" => {
                let browser = current(instance, &session, &connection)
                    .ok_or_else(|| RpcFailure::unavailable("Browser attachment is unavailable; its network connections were closed."))?;
                let tunnel = input["tunnelID"].as_str().unwrap_or_default();
                let outcome =
                    match method {
                        "tunnel.open" => {
                            let host = input["target"]["host"].as_str().unwrap_or_default();
                            let port = input["target"]["port"].as_u64().unwrap_or(0) as u16;
                            browser.tunnels.open(host, port).await.map(Value::String)
                        }
                        "tunnel.read" => browser.tunnels.read(tunnel).await.map(
                            |chunk| json!({ "data": base64::engine::general_purpose::STANDARD.encode(&chunk.data), "eof": chunk.eof }),
                        ),
                        "tunnel.write" => {
                            let data = base64::engine::general_purpose::STANDARD
                                .decode(input["data"].as_str().unwrap_or_default())
                                .unwrap_or_default();
                            let end = input["end"].as_bool().unwrap_or(false);
                            browser.tunnels.write(tunnel, &data, end).await.map(|_| Value::Null)
                        }
                        _ => {
                            browser.tunnels.close(tunnel);
                            Ok(Value::Null)
                        }
                    };
                outcome.map_err(RpcFailure::unavailable)
            }
            _ => Err(RpcFailure::system("rpc.method_not_found", format!("Unknown RPC method: {RPC_ID}.{method}"))),
        }
    }

    /// `attach`: registers the desktop, grants the browser to the session and waits for the end.
    async fn attach(self: &Arc<Self>, instance: &Arc<Instance>, session_id: &str, connection: &str) -> Result<Value, RpcFailure> {
        let session = self.sessions.get(session_id).map_err(|_| RpcFailure::unavailable("Session not found."))?;
        if crate::location::canonical_directory(&session.location.directory) != instance.key {
            return Err(RpcFailure::unavailable("Session belongs to another location."));
        }
        if !*instance.active.lock() {
            return Err(RpcFailure::unavailable("Browser is unavailable."));
        }
        // A replaced attachment ends and removes its own allows before this one adds its own.
        let previous = instance.browsers.lock().remove(session_id);
        if let Some(previous) = previous {
            previous.finish(End::Replaced);
            self.release(session_id, &previous);
        }
        let browser = Arc::new(Attachment::new(connection));
        instance.browsers.lock().insert(session_id.to_string(), browser.clone());
        // Interrupted (the desktop went away): remove the allows, then end the attachment if it is current.
        let guard =
            AttachGuard { browsers: self.clone(), instance: instance.clone(), session: session_id.to_string(), browser: browser.clone() };
        self.grant(session_id, &browser);
        self.publish(instance, json!({ "type": "attached", "connectionID": connection, "version": VERSION }));
        let end = browser.ended().await;
        drop(guard);
        match end {
            End::Closed => Ok(json!("closed")),
            End::Replaced => Ok(json!("replaced")),
            End::Unavailable => Err(RpcFailure::rpc_unavailable()),
        }
    }

    fn grant(&self, session_id: &str, browser: &Attachment) {
        let Ok(session) = self.sessions.get(session_id) else { return };
        let mut permissions = session.permissions.unwrap_or_default();
        permissions.extend(access(Effect::Allow));
        if self.sessions.set_permissions(session_id, permissions).is_ok() {
            *browser.granted.lock() = true;
        }
    }

    /// Removes one of each of the attachment's allows, publishing only when something changed.
    fn release(&self, session_id: &str, browser: &Attachment) {
        if !std::mem::replace(&mut *browser.granted.lock(), false) {
            return;
        }
        let Ok(session) = self.sessions.get(session_id) else { return };
        let current = session.permissions.unwrap_or_default();
        let mut permissions = current.clone();
        for allow in access(Effect::Allow) {
            if let Some(index) =
                permissions.iter().position(|r| r.action == allow.action && r.resource == allow.resource && r.effect == Effect::Allow)
            {
                permissions.remove(index);
            }
        }
        if permissions.len() != current.len() {
            let _ = self.sessions.set_permissions(session_id, permissions);
        }
    }

    /// `connection.target(sessionID, action)`: the session's attachment and the action's tab.
    pub(crate) fn target(&self, key: &str, session_id: &str, action: &Value) -> Result<Target, oc_tools::ToolError> {
        let instance = self.instances.lock().get(key).cloned();
        let browser = instance.as_ref().and_then(|i| i.browsers.lock().get(session_id).cloned());
        let (Some(instance), Some(browser)) = (instance, browser) else {
            return Err(oc_tools::ToolError::new(
                "[browser.disconnected] No desktop browser is connected to this session. Open this session in the desktop app and wait for it to connect. Then call browser.tabs.list({}). Repeating browser actions while disconnected will not help.",
            ));
        };
        let tab = match action.get("tabID").and_then(Value::as_str) {
            None => None,
            Some(id) => {
                let state = browser.state.lock();
                let tab = state["tabs"].as_array().and_then(|tabs| tabs.iter().find(|t| t["id"] == id).cloned());
                match tab {
                    Some(tab) => Some(tab),
                    None => {
                        return Err(oc_tools::ToolError::new(
                            "[browser.tab_unavailable] This tab is closed or does not belong to the connected session. Call browser.tabs.list({}) and use an exact returned tabID. If no tabs exist, use browser.tabs.open({}). Never substitute a request ID, file ID, or element ref for tabID.",
                        ));
                    }
                }
            }
        };
        Ok(Target { instance, browser, tab })
    }

    /// `request(...)`: announces the command, waits for its result, the attachment's end or the timeout.
    pub(crate) async fn request(
        &self,
        target: &Target,
        action: &Value,
        uploads: Vec<files::File>,
    ) -> Result<BrowserResult, oc_tools::ToolError> {
        let kind = action["type"].as_str().unwrap_or_default().to_string();
        let request = uuid::Uuid::new_v4().to_string();
        let (tx, rx) = oneshot::channel();
        let command = encode_command(action, target.tab.as_ref(), &uploads);
        target.browser.pending.lock().insert(request.clone(), Pending { command, result: Some(tx) });
        // `Effect.onInterrupt` (cancel) and `Effect.ensuring` (forget the request).
        let mut guard = RequestGuard { browsers: self, target, request: request.clone(), cancel: true };
        self.publish(&target.instance, json!({ "type": "command", "connectionID": target.browser.connection_id, "requestID": request }));
        let timeout = *self.timeout.lock();
        let outcome = tokio::select! {
            result = rx => Some(result.unwrap_or_else(|_| Err(String::new()))),
            _ = target.browser.ended() => None,
            _ = tokio::time::sleep(timeout) => {
                drop(guard);
                return Err(oc_tools::ToolError::new(format!(
                    "[browser.timeout] browser.{kind} did not finish within {} seconds; its outcome is unknown. Check the desktop connection, call browser.tabs.list({{}}), and inspect the tab or browser.files.list({{tabID}}) for completed work. Do not blindly repeat a mutating action or start another recording.",
                    REQUEST_TIMEOUT.as_secs()
                )));
            }
        };
        guard.cancel = false;
        drop(guard);
        match outcome {
            Some(Ok(result)) => Ok(result),
            Some(Err(message)) => Err(oc_tools::ToolError::new(message)),
            None => Err(oc_tools::ToolError::new(
                "[browser.disconnected] Browser connection closed; the action may already have run. Reconnect this session in the desktop app, call browser.tabs.list({}), and inspect the target tab with browser.snapshot({tabID}). Do not repeat clicks, submissions, uploads, or evaluations until their outcome is known.",
            )),
        }
    }
}

/// The attachment and tab a tool call targets.
pub(crate) struct Target {
    instance: Arc<Instance>,
    browser: Arc<Attachment>,
    tab: Option<Value>,
}

struct RequestGuard<'a> {
    browsers: &'a Browsers,
    target: &'a Target,
    request: String,
    cancel: bool,
}

impl Drop for RequestGuard<'_> {
    fn drop(&mut self) {
        if self.cancel {
            let data = json!({ "type": "cancel", "connectionID": self.target.browser.connection_id, "requestID": self.request });
            self.browsers.publish(&self.target.instance, data);
        }
        self.target.browser.pending.lock().remove(&self.request);
    }
}

struct AttachGuard {
    browsers: Arc<Browsers>,
    instance: Arc<Instance>,
    session: String,
    browser: Arc<Attachment>,
}

impl Drop for AttachGuard {
    fn drop(&mut self) {
        self.browsers.release(&self.session, &self.browser);
        let current = self.instance.browsers.lock().get(&self.session).is_some_and(|b| Arc::ptr_eq(b, &self.browser));
        if current {
            self.instance.close(&self.session, End::Closed);
        }
    }
}

/// The current attachment of the session when `connection` owns it.
fn current(instance: &Instance, session: &str, connection: &str) -> Option<Arc<Attachment>> {
    instance.browsers.lock().get(session).filter(|b| b.connection_id == connection).cloned()
}

/// The browser tools and their skill, which an attachment allows together.
fn access(effect: Effect) -> [PermissionRule; 2] {
    let rule = |action: &str, resource: &str| PermissionRule { action: action.into(), resource: resource.into(), effect: effect.clone() };
    [rule("browser", "*"), rule("skill", "browser")]
}

/// The `Browser.Command` the desktop retrieves: the action (`type` first, like its schema), the tab's
/// generation and the upload bytes. Uploads carry the files' names instead of server paths.
fn encode_command(action: &Value, tab: Option<&Value>, uploads: &[files::File]) -> Value {
    let kind = action["type"].as_str().unwrap_or_default();
    let mut encoded = Map::new();
    encoded.insert("type".into(), json!(kind));
    let properties = tools::operation(kind).and_then(|op| op.input_schema["properties"].as_object().cloned()).unwrap_or_default();
    for key in properties.keys() {
        if let Some(value) = action.get(key) {
            encoded.insert(key.clone(), value.clone());
        }
    }
    if matches!(kind, "files.upload" | "files.drop") {
        encoded.insert("paths".into(), json!(uploads.iter().map(|f| f.name.clone()).collect::<Vec<_>>()));
    }
    let mut command = Map::new();
    command.insert("action".into(), Value::Object(encoded));
    if let Some(generation) = tab.and_then(|t| t.get("generation")) {
        command.insert("generation".into(), generation.clone());
    }
    let engine = base64::engine::general_purpose::STANDARD;
    let files: Vec<Value> =
        uploads.iter().map(|f| json!({ "id": f.id, "name": f.name, "mime": f.mime, "data": engine.encode(&f.data) })).collect();
    command.insert("files".into(), Value::Array(files));
    Value::Object(command)
}

/// Decodes a successful outcome's `Browser.Result` (file bytes from base64).
fn decode_result(result: &Value) -> Result<BrowserResult, String> {
    let engine = base64::engine::general_purpose::STANDARD;
    let mut files = Vec::new();
    for (index, file) in result["files"].as_array().into_iter().flatten().enumerate() {
        let data = engine
            .decode(file["data"].as_str().unwrap_or_default())
            .map_err(|_| format!("Invalid base64 string\n  at [\"outcome\"][\"result\"][\"files\"][{index}][\"data\"]"))?;
        if data.len() > MAX_FILE_BYTES {
            return Err(format!(
                "Expected a value with a length of at most {MAX_FILE_BYTES}\n  at [\"outcome\"][\"result\"][\"files\"][{index}][\"data\"]"
            ));
        }
        files.push(files::File {
            id: file["id"].as_str().unwrap_or_default().into(),
            name: file["name"].as_str().unwrap_or_default().into(),
            mime: file["mime"].as_str().unwrap_or_default().into(),
            data,
        });
    }
    Ok(BrowserResult { value: result["value"].clone(), files })
}

/// The first issue of decoding `input` against an RPC input schema, as Effect's `SchemaError` reads.
fn input_issue(schema: &Value, input: &Value) -> Option<String> {
    let issues = oc_tools::validate::validate(schema, input, oc_tools::validate::Flavor::Effect);
    let issue = issues.first()?;
    let path: String = issue
        .path
        .iter()
        .map(|segment| match segment {
            oc_tools::validate::PathSegment::Key(key) => format!("[{}]", serde_json::to_string(key).unwrap_or_default()),
            oc_tools::validate::PathSegment::Index(index) => format!("[{index}]"),
        })
        .collect();
    Some(if path.is_empty() { issue.message.clone() } else { format!("{}\n  at {path}", issue.message) })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn issue(method: &str, input: Value) -> Option<String> {
        input_issue(input_schema(method).unwrap(), &input)
    }

    #[test]
    fn rpc_inputs_fail_like_effect() {
        let tab = "tab_00000000-0000-4000-8000-000000000001";
        assert_eq!(
            issue("attach", json!({"sessionID": "ses_1", "connectionID": "c", "version": 3})).unwrap(),
            "Expected 4\n  at [\"version\"]"
        );
        assert_eq!(
            issue("attach", json!({"sessionID": "nope", "connectionID": "c", "version": 4})).unwrap(),
            "Expected a string starting with \"ses\"\n  at [\"sessionID\"]"
        );
        assert_eq!(
            issue("attach", json!({"sessionID": 1, "connectionID": "c", "version": 4})).unwrap(),
            "Expected Session.ID\n  at [\"sessionID\"]"
        );
        assert_eq!(issue("attach", json!({})).unwrap(), "Missing key\n  at [\"sessionID\"]");
        assert_eq!(issue("attach", Value::Null).unwrap(), "Expected object");
        assert_eq!(
            issue("state", json!({"sessionID": "ses_1", "connectionID": "c", "state": {"tabs": [{"id": "nope"}], "focusedTabID": null}}))
                .unwrap(),
            "Expected a string matching the RegExp ^tab_[a-f0-9-]{36}$\n  at [\"state\"][\"tabs\"][0][\"id\"]"
        );
        assert_eq!(
            issue("tunnel.open", json!({"sessionID": "ses_1", "connectionID": "c", "target": {"host": "127.0.0.1", "port": 0}})).unwrap(),
            "Expected a value between 1 and 65535\n  at [\"target\"][\"port\"]"
        );
        assert_eq!(
            issue("tunnel.open", json!({"sessionID": "ses_1", "connectionID": "c", "target": {"host": "a b", "port": 80}})).unwrap(),
            "Expected a string matching the RegExp ^[a-zA-Z0-9._:%-]+$\n  at [\"target\"][\"host\"]"
        );
        let state = json!({"tabs": [{"id": tab, "url": "about:blank", "title": "", "loading": false, "canGoBack": false, "canGoForward": false, "generation": 0}], "focusedTabID": tab});
        assert_eq!(issue("state", json!({"sessionID": "ses_1", "connectionID": "c", "state": state})), None);
    }

    #[test]
    fn commands_put_the_type_first_and_send_upload_names() {
        let action = json!({"tabID": "tab_x", "ref": "e1", "paths": ["dir/a.txt"], "type": "files.upload", "extra": 1});
        let tab = json!({"generation": 3});
        let upload = files::File { id: "file_1".into(), name: "a.txt".into(), mime: "text/plain".into(), data: b"hi".to_vec() };
        let command = encode_command(&action, Some(&tab), &[upload]);
        assert_eq!(
            serde_json::to_string(&command).unwrap(),
            r#"{"action":{"type":"files.upload","tabID":"tab_x","ref":"e1","paths":["a.txt"]},"generation":3,"files":[{"id":"file_1","name":"a.txt","mime":"text/plain","data":"aGk="}]}"#
        );
        assert_eq!(
            serde_json::to_string(&encode_command(&json!({"type": "tabs.list"}), None, &[])).unwrap(),
            r#"{"action":{"type":"tabs.list"},"files":[]}"#
        );
    }
}
