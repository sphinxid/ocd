//! The built-in `opencode.browser` plugin (`packages/plugin-browser`, 2.2): the desktop app's browser
//! attaches to a session over the `experimental.browser` RPC, and the `browser.*` Code Mode tools drive
//! it through a command/result round trip.
//!
//! - `connection.ts` → [`Browsers`], the driver of `machine.ts` → [`machine`] (2.2.7): one attachment per
//!   session and location. `attach` stays pending for the attachment's lifetime and answers `closed` or
//!   `replaced`; while it is attached the session carries exactly one set of `allow` rules for the browser
//!   tools and the `browser` skill, which every agent denies (`location::browser_denies`). The grant is
//!   written before the desktop hears `attached`, and detaching removes every such rule. `state` stores the
//!   desktop's tab inventory, `command` hands out a pending request, `result` completes it. Control events
//!   (`rpc.experimental.browser.control`) announce the attachment, each request ID and cancellations;
//!   arguments and results never go on the event feed.
//! - `tools.ts` → [`tools`]: the 45 operations of `Browser.Operations` as Code Mode tools, with the
//!   definitions upstream's tool runtime exports (`assets/browser/schemas.json`, regenerated with
//!   `scripts/browser-schemas.ts`), URL normalization, result validation and capture export.
//! - `files.ts` → [`files`], `tunnel.ts` → [`tunnel`] (TCP relays for the desktop's proxy).
//!
//! `proxy.ts` is the desktop's own HTTP/CONNECT listener in front of the tunnels; the server never loads it.

pub mod files;
pub mod machine;
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
use machine::{ConnectionState, Effect as MachineEffect, Event, Outcome, SyncResult};

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

/// `Browser.Result`: the desktop's JSON value and its files.
#[derive(Debug, Clone, PartialEq)]
pub struct BrowserResult {
    pub value: Value,
    pub files: Vec<files::File>,
}

/// One `attach` call's handle: its tunnels and how it ended.
struct Handle {
    end: watch::Sender<Option<Outcome>>,
    tunnels: tunnel::Tunnels,
}

impl Handle {
    fn new() -> Self {
        Handle { end: watch::channel(None).0, tunnels: tunnel::Tunnels::new() }
    }

    fn finish(&self, outcome: Outcome) {
        self.tunnels.dispose();
        self.end.send_if_modified(|current| {
            if current.is_some() {
                return false;
            }
            *current = Some(outcome);
            true
        });
    }

    async fn ended(&self) -> Outcome {
        let mut rx = self.end.subscribe();
        loop {
            if let Some(outcome) = *rx.borrow_and_update() {
                return outcome;
            }
            if rx.changed().await.is_err() {
                return Outcome::Closed;
            }
        }
    }
}

/// The plugin instance of one location (`BrowserConnection.make`): the machine's state, the handles of its
/// `attach` calls by attempt and the waiting requests.
struct Instance {
    key: String,
    location: LocationRef,
    /// The location's RPC service closed (`Rpc.close`): every call fails `rpc.unavailable`.
    closed: CancellationToken,
    /// The machine; effects run while it is held, so controls reach the bus in transition order.
    machine: Mutex<ConnectionState>,
    attachments: Mutex<HashMap<u64, Arc<Handle>>>,
    requests: Mutex<HashMap<String, oneshot::Sender<Result<BrowserResult, String>>>>,
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
                    let session = session.to_string();
                    let event =
                        if event.kind == "session.deleted" { Event::SessionDeleted { session } } else { Event::SessionMoved { session } };
                    me.dispatch(&instance, event);
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
                    closed: CancellationToken::new(),
                    machine: Mutex::new(ConnectionState::default()),
                    attachments: Mutex::new(HashMap::new()),
                    requests: Mutex::new(HashMap::new()),
                })
            })
            .clone()
    }

    /// The location's RPC service closed (eviction, shutdown): pending calls fail with `rpc.unavailable`, the
    /// plugin is disposed (its attachments close and revoke their rules) and a later call starts a fresh instance.
    pub fn close_location(self: &Arc<Self>, key: &str) {
        let Some(instance) = self.instances.lock().remove(key) else { return };
        instance.closed.cancel();
        self.dispatch(&instance, Event::Disposed);
    }

    /// The plugin restarted or was disabled at the location (its scope closed): every attachment ends with
    /// `closed` and revokes its rules, and the next call reaches a fresh instance.
    pub fn restart(self: &Arc<Self>, key: &str) {
        let Some(instance) = self.instances.lock().remove(key) else { return };
        self.dispatch(&instance, Event::Disposed);
    }

    /// Whether a session at the location has a desktop attached.
    pub fn attached(&self, key: &str, session_id: &str) -> bool {
        self.instances
            .lock()
            .get(key)
            .is_some_and(|i| matches!(i.machine.lock().sessions.get(session_id), Some(machine::SessionState::Attached { .. })))
    }

    /// Feeds one event to the instance's machine and performs its effects.
    fn dispatch(self: &Arc<Self>, instance: &Arc<Instance>, event: Event) {
        let mut state = instance.machine.lock();
        self.step(instance, &mut state, event);
    }

    fn step(self: &Arc<Self>, instance: &Arc<Instance>, state: &mut ConnectionState, event: Event) {
        // Whether a tool call waiting on a request was woken: it resumes before the forked sync runs.
        let mut woken = false;
        for effect in machine::transition(state, event) {
            match effect {
                MachineEffect::SyncPermissions { session, attempt, allow } => {
                    self.sync_permissions(instance, session, attempt, allow, woken)
                }
                MachineEffect::CloseAttachment { attempt, outcome } => {
                    let handle = instance.attachments.lock().remove(&attempt);
                    if let Some(handle) = handle {
                        handle.finish(outcome);
                    }
                }
                MachineEffect::EmitControl(control) => {
                    self.bus.publish(CONTROL, control, Some(instance.location.clone()));
                }
                MachineEffect::SettleRequest { request, settlement } => {
                    let sender = instance.requests.lock().remove(&request);
                    if let Some(sender) = sender {
                        woken |= sender.send(settlement).is_ok();
                    }
                }
                MachineEffect::Warn { message, error } => tracing::warn!(error = error.unwrap_or_default(), "{message}"),
                MachineEffect::NotifyDrained => {}
            }
        }
    }

    /// `syncPermissions`: reads the session and, when its browser rules differ, replaces them; the outcome goes back
    /// to the machine. Runs in the background, like upstream's fork into the plugin's `work` scope.
    ///
    /// A forked fiber runs once the running ones yield. When the same transition woke a tool call that waited on a
    /// request (`woken`), that call's fiber goes first and carries the session's drain on without yielding until it
    /// waits for the provider again: the sync then runs only once the session's next step sent its request (its
    /// context already read), or once its execution ended.
    fn sync_permissions(self: &Arc<Self>, instance: &Arc<Instance>, session: String, attempt: u64, allow: bool, woken: bool) {
        let gate = woken.then(|| crate::yields::Yield::register(&self.bus, &session));
        let (me, instance) = (self.clone(), instance.clone());
        let sync = {
            let (sessions, session) = (self.sessions.clone(), session.clone());
            move || -> Result<(), String> {
                let current = sessions.get(&session).map_err(|e| e.to_string())?;
                match machine::reconcile(current.permissions.as_deref().unwrap_or(&[]), allow) {
                    Some(permissions) => sessions.set_permissions(&session, permissions).map_err(|e| e.to_string()),
                    None => Ok(()),
                }
            }
        };
        let Ok(runtime) = tokio::runtime::Handle::try_current() else {
            std::thread::spawn(move || {
                let result = match sync() {
                    Ok(()) => SyncResult::Synced,
                    Err(error) => SyncResult::Failed(error),
                };
                me.dispatch(&instance, Event::Permissions { session, attempt, result });
            });
            return;
        };
        runtime.spawn(async move {
            if let Some(gate) = gate {
                gate.wait().await;
            }
            let result = match tokio::time::timeout(machine::PERMISSION_SYNC_TIMEOUT, tokio::task::spawn_blocking(sync)).await {
                Ok(Ok(Ok(()))) => SyncResult::Synced,
                Ok(Ok(Err(error))) => SyncResult::Failed(error),
                Ok(Err(error)) => SyncResult::Failed(error.to_string()),
                Err(_) => SyncResult::TimedOut,
            };
            me.dispatch(&instance, Event::Permissions { session, attempt, result });
        });
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
        // A long-lived call releases its location when the routing cache invalidates it.
        tokio::select! {
            biased;
            _ = closed.cancelled() => Err(RpcFailure::rpc_unavailable()),
            result = handler => result,
        }
    }

    async fn handle(self: &Arc<Self>, instance: &Arc<Instance>, method: &str, input: &Value) -> Result<Value, RpcFailure> {
        let session = input["sessionID"].as_str().unwrap_or_default().to_string();
        let connection = input["connectionID"].as_str().unwrap_or_default().to_string();
        match method {
            "attach" => self.attach(instance, &session, &connection).await,
            "state" => {
                let state = schema::decode(&SCHEMAS["rpc"]["state"]["$defs"]["Browser.State"], &input["state"])
                    .unwrap_or(json!({ "tabs": [], "focusedTabID": null }));
                let mut machine = instance.machine.lock();
                if !machine::is_attached_connection(&machine, &session, &connection) {
                    return Err(RpcFailure::unavailable("Browser is unavailable."));
                }
                self.step(instance, &mut machine, Event::StateUpdated { session, connection, state });
                Ok(Value::Null)
            }
            "command" => {
                let request = input["requestID"].as_str().unwrap_or_default();
                machine::select_pending_command(&instance.machine.lock(), &session, &connection, request)
                    .ok_or_else(|| RpcFailure::unavailable("Browser request is no longer available."))
            }
            "result" => {
                let request = input["requestID"].as_str().unwrap_or_default().to_string();
                let outcome = &input["outcome"];
                // The outcome decodes with the input, before the handler runs.
                let outcome = if outcome["type"] == "failure" {
                    Err(format!(
                        "[browser.{}] {}",
                        outcome["code"].as_str().unwrap_or_default(),
                        outcome["message"].as_str().unwrap_or_default()
                    ))
                } else {
                    Ok(decode_result(&outcome["result"]).map_err(|m| RpcFailure::system("rpc.invalid_input", m))?)
                };
                let mut machine = instance.machine.lock();
                if !machine::is_attached_connection(&machine, &session, &connection) {
                    return Err(RpcFailure::unavailable("Browser is unavailable."));
                }
                self.step(instance, &mut machine, Event::RequestSettled { session, connection, request, outcome });
                Ok(Value::Null)
            }
            "tunnel.open" | "tunnel.read" | "tunnel.write" | "tunnel.close" => {
                let handle = machine::select_attachment(&instance.machine.lock(), &session, &connection)
                    .and_then(|attempt| instance.attachments.lock().get(&attempt).cloned())
                    .ok_or_else(|| RpcFailure::unavailable("Browser attachment is unavailable; its network connections were closed."))?;
                let tunnel = input["tunnelID"].as_str().unwrap_or_default();
                let outcome =
                    match method {
                        "tunnel.open" => {
                            let host = input["target"]["host"].as_str().unwrap_or_default();
                            let port = input["target"]["port"].as_u64().unwrap_or(0) as u16;
                            handle.tunnels.open(host, port).await.map(Value::String)
                        }
                        "tunnel.read" => handle.tunnels.read(tunnel).await.map(
                            |chunk| json!({ "data": base64::engine::general_purpose::STANDARD.encode(&chunk.data), "eof": chunk.eof }),
                        ),
                        "tunnel.write" => {
                            let data = base64::engine::general_purpose::STANDARD
                                .decode(input["data"].as_str().unwrap_or_default())
                                .unwrap_or_default();
                            let end = input["end"].as_bool().unwrap_or(false);
                            handle.tunnels.write(tunnel, &data, end).await.map(|_| Value::Null)
                        }
                        _ => {
                            handle.tunnels.close(tunnel);
                            Ok(Value::Null)
                        }
                    };
                outcome.map_err(RpcFailure::unavailable)
            }
            _ => Err(RpcFailure::system("rpc.method_not_found", format!("Unknown RPC method: {RPC_ID}.{method}"))),
        }
    }

    /// `attach`: registers the desktop's attempt and waits for its end. The machine grants the browser to the
    /// session and announces `attached` once that write settled.
    async fn attach(self: &Arc<Self>, instance: &Arc<Instance>, session_id: &str, connection: &str) -> Result<Value, RpcFailure> {
        let session = self.sessions.get(session_id).map_err(|_| RpcFailure::unavailable("Session not found."))?;
        if crate::location::canonical_directory(&session.location.directory) != instance.key {
            return Err(RpcFailure::unavailable("Session belongs to another location."));
        }
        let (attempt, handle) = {
            let mut machine = instance.machine.lock();
            if machine.disposed {
                return Err(RpcFailure::unavailable("Browser is unavailable."));
            }
            let attempt = machine.next_attempt;
            let handle = Arc::new(Handle::new());
            instance.attachments.lock().insert(attempt, handle.clone());
            let event = Event::AttachStarted { session: session_id.to_string(), attempt, connection: connection.to_string() };
            self.step(instance, &mut machine, event);
            (attempt, handle)
        };
        // However the call ends (the desktop went away, or the attachment closed), the machine hears of it.
        let _release = AttachRelease { browsers: self.clone(), instance: instance.clone(), session: session_id.to_string(), attempt };
        match handle.ended().await {
            Outcome::Closed => Ok(json!("closed")),
            Outcome::Replaced => Ok(json!("replaced")),
            Outcome::Unavailable => Err(RpcFailure::unavailable("Browser permissions could not be granted.")),
        }
    }

    /// `connection.target(sessionID, action)`: the session's attachment and the action's tab.
    pub(crate) fn target(&self, key: &str, session_id: &str, action: &Value) -> Result<Target, oc_tools::ToolError> {
        let instance = self.instances.lock().get(key).cloned();
        let selected = match &instance {
            Some(instance) => machine::select_target(&instance.machine.lock(), session_id, action),
            None => machine::select_target(&ConnectionState::default(), session_id, action),
        };
        let (connection, tab) = selected.map_err(oc_tools::ToolError::new)?;
        let instance = instance.expect("an attached session has an instance");
        Ok(Target { instance, session: session_id.to_string(), connection, tab })
    }

    /// `request(...)`: announces the command and waits for the machine to settle it (a result, the attachment's
    /// end, the timeout). Requests go to the attachment the target selected; one that closed meanwhile runs nothing.
    pub(crate) async fn request(
        self: &Arc<Self>,
        target: &Target,
        action: &Value,
        uploads: Vec<files::File>,
    ) -> Result<BrowserResult, oc_tools::ToolError> {
        let request = uuid::Uuid::new_v4().to_string();
        let (tx, mut rx) = oneshot::channel();
        let command = encode_command(action, target.tab.as_ref(), &uploads);
        target.instance.requests.lock().insert(request.clone(), tx);
        // `Effect.onInterrupt` (cancel) and `Effect.ensuring` (forget the request).
        let mut guard = RequestGuard { browsers: self.clone(), target, request: request.clone(), settled: false };
        let ids = |request: &str| (target.session.clone(), target.connection.clone(), request.to_string());
        let (session, connection, id) = ids(&request);
        self.dispatch(&target.instance, Event::RequestStarted { session, connection, request: id, command });
        let timeout = *self.timeout.lock();
        let result = match tokio::time::timeout(timeout, &mut rx).await {
            Ok(result) => result,
            Err(_) => {
                let (session, connection, id) = ids(&request);
                self.dispatch(&target.instance, Event::RequestTimedOut { session, connection, request: id });
                rx.await
            }
        };
        guard.settled = true;
        drop(guard);
        match result {
            Ok(Ok(result)) => Ok(result),
            Ok(Err(message)) => Err(oc_tools::ToolError::new(message)),
            Err(_) => Err(oc_tools::ToolError::new(machine::disconnected_after_dispatch())),
        }
    }
}

/// The attachment and tab a tool call targets.
pub(crate) struct Target {
    instance: Arc<Instance>,
    session: String,
    connection: String,
    tab: Option<Value>,
}

struct RequestGuard<'a> {
    browsers: Arc<Browsers>,
    target: &'a Target,
    request: String,
    settled: bool,
}

impl Drop for RequestGuard<'_> {
    fn drop(&mut self) {
        if !self.settled {
            let event = Event::RequestCancelled {
                session: self.target.session.clone(),
                connection: self.target.connection.clone(),
                request: self.request.clone(),
            };
            self.browsers.dispatch(&self.target.instance, event);
        }
        self.target.instance.requests.lock().remove(&self.request);
    }
}

struct AttachRelease {
    browsers: Arc<Browsers>,
    instance: Arc<Instance>,
    session: String,
    attempt: u64,
}

impl Drop for AttachRelease {
    fn drop(&mut self) {
        let event = Event::AttachClosed { session: self.session.clone(), attempt: self.attempt };
        self.browsers.dispatch(&self.instance, event);
    }
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

/// The first issue of decoding `input` against an RPC input schema (or an operation's output schema), as
/// Effect's `SchemaError` message reads (`SchemaIssue.makeFormatterDefault`).
pub(crate) fn input_issue(schema: &Value, input: &Value) -> Option<String> {
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
