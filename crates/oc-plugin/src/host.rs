//! [`PluginHost`]: the API the core uses. Starts the sidecar lazily (only when a location needs a
//! package/local plugin), restarts it with backoff after a crash (replaying every location's last
//! load), mirrors the sidecar's registrations (hooks, transform domains, RPC ids, inventory) so
//! hot paths can skip IPC, and serves the sidecar's calls through [`PluginApi`].

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Weak};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use indexmap::IndexMap;
use parking_lot::Mutex;
use serde_json::{Value, json};
use tokio::sync::broadcast;

use crate::aisdk::Streams;
use crate::api::{ApiError, ApiRequest, PluginApi};
use crate::bun::BunResolver;
use crate::npm::Npm;
use crate::process::{CallError, Incoming, Sidecar};
use crate::protocol::{Activation, HookSpec, LocationRef, ModuleLoad, Operation, WireError, PROTOCOL_VERSION};
use crate::supervisor::{self, InternalPlugins, LoadOutcome, Loaded, Resolution};

/// The sidecar bundle, built by scripts/build-plugin-host.sh.
pub const HOST_JS: &str = include_str!("../host/dist/host.js");

/// Static configuration.
#[derive(Debug, Clone)]
pub struct HostOptions {
    /// `Global.bin` (`<cache>/bin`): Bun and the extracted bundle live here.
    pub bin: PathBuf,
    /// `Global.cache`: npm packages go to `<cache>/npm`.
    pub cache: PathBuf,
    /// Cross-process lock root (`Global.locks()`).
    pub locks: PathBuf,
    /// Working directory of the sidecar process.
    pub cwd: PathBuf,
    /// `ctx.app`: `{name, version, channel}`.
    pub app: Value,
}

impl HostOptions {
    pub fn from_global(global: &oc_util::global::Global, app: Value) -> Self {
        Self { bin: global.bin.clone(), cache: global.cache.clone(), locks: global.locks(), cwd: global.home.clone(), app }
    }
}

/// Notifications for the core (`PluginHost::subscribe`).
#[derive(Debug, Clone, PartialEq)]
pub enum HostEvent {
    /// Plugin transform registrations of these domains changed: rebuild them.
    DomainsChanged { location: LocationRef, domains: Vec<String> },
    HooksChanged { location: LocationRef },
    RpcChanged { location: LocationRef },
    /// The plugin inventory changed (publish `plugin.updated`).
    PluginsChanged { location: LocationRef },
    /// A local plugin source file to watch; on change, call `load` again for its locations.
    Watch { file: PathBuf },
    /// The sidecar died; it will be restarted.
    Crashed { status: String },
    /// The sidecar was restarted and every location reloaded; rebuild all plugin domains.
    Restarted,
}

/// Managed-policy check: true when `plugin:<target>` is denied.
pub type BlockedFn = Arc<dyn Fn(&str) -> bool + Send + Sync>;
/// Receives `context.progress(metadata)` calls of a plugin tool.
pub type ProgressFn = Arc<dyn Fn(Value) + Send + Sync>;

/// Inputs of [`PluginHost::load`].
#[derive(Clone)]
pub struct LoadRequest {
    pub location: LocationRef,
    /// `Location.Info` JSON (`{directory, workspaceID?, project: {id, directory, canonical}}`), `ctx.location`.
    pub info: Value,
    /// Configured operations in order (discovered directory plugins first).
    pub operations: Vec<Operation>,
    pub internal: InternalPlugins,
    /// Managed policy: returns true when `plugin:<target>` is denied (`integration.use`).
    pub blocked: Option<BlockedFn>,
}

impl std::fmt::Debug for LoadRequest {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LoadRequest").field("location", &self.location).field("operations", &self.operations).field("internal", &self.internal).finish()
    }
}

/// Result of [`PluginHost::load`].
#[derive(Debug, Clone, Default)]
pub struct LoadResult {
    /// Enabled plugins in boot order (internal ids and packages) plus load failures.
    pub resolution: Resolution,
    /// Sidecar inventory: one `Plugin.Info` per activated package (boot order), then failures.
    pub packages: Vec<Value>,
}

impl LoadResult {
    /// Full `Plugin.Info` inventory in upstream order (active plugins in boot order, then
    /// failures); `builtin` gives the info of an internal id.
    pub fn inventory(&self, builtin: impl Fn(&str) -> Value) -> Vec<Value> {
        let mut out = Vec::new();
        for entry in &self.resolution.plugins {
            match entry {
                supervisor::Entry::Internal(id) => out.push(builtin(id)),
                supervisor::Entry::Package(p) => out.push(
                    self.packages.iter().find(|info| info.get("id").and_then(Value::as_str) == Some(p.id.as_str())).cloned().unwrap_or_else(|| {
                        let mut features = json!({ "server": true });
                        if let (Some(out), Some(extra)) = (features.as_object_mut(), p.features.as_object()) {
                            out.extend(extra.clone());
                        }
                        json!({ "id": p.id, "source": p.source, "state": { "status": "active" }, "features": features })
                    }),
                ),
            }
        }
        out.extend(self.resolution.failures.iter().cloned());
        out
    }
}

/// A runtime hook failure.
#[derive(Debug, Clone, PartialEq, thiserror::Error)]
pub enum HookError {
    /// `tool.execute.before` rejected the call with a `Tool.Error`.
    #[error("{message}")]
    Tool { message: String, metadata: Option<Value>, cause: Option<(String, String)> },
    /// A hook declined (a permission it asserted was rejected, or a question was dismissed).
    #[error("{}", .0.message())]
    Declined(Decline),
    /// A hook defect (or the sidecar failed): upstream fails the operation.
    #[error("{0}")]
    Failed(String),
}

/// A decline that crossed the sidecar: upstream's `Permission.DeclinedError` or
/// `QuestionTool.CancelledError`, thrown directly or rethrown by a plugin wrapper and attached to the
/// `Tool.Error` it failed with (143807327). The session treats it as a decline, not a tool failure.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Decline {
    /// The user rejected a permission request.
    Permission,
    /// The user dismissed a question.
    Question,
}

impl Decline {
    /// The error name a decline crosses the boundary with.
    pub fn name(&self) -> &'static str {
        match self {
            Decline::Permission => "Permission.DeclinedError",
            Decline::Question => "QuestionTool.CancelledError",
        }
    }

    pub fn message(&self) -> &'static str {
        match self {
            Decline::Permission => "The user declined this tool call",
            Decline::Question => "The user dismissed this question",
        }
    }

    /// A sidecar failure of type `declined` (`data.kind`).
    fn from_wire(kind: Option<&str>, data: Option<&Value>) -> Option<Decline> {
        if kind != Some("declined") {
            return None;
        }
        Some(match data.and_then(|d| d.get("kind")).and_then(Value::as_str) {
            Some("question") => Decline::Question,
            _ => Decline::Permission,
        })
    }
}

/// A plugin tool failure (`Tool.Error`), or a defect.
#[derive(Debug, Clone, PartialEq, thiserror::Error)]
#[error("{message}")]
pub struct ToolError {
    pub message: String,
    pub metadata: Option<Value>,
    /// True for `Tool.Error` (model-visible); false for defects and host failures.
    pub tool_error: bool,
    /// Set when the tool (or a native tool it wrapped) declined.
    pub declined: Option<Decline>,
    /// The session error upstream's `toSessionError` derives from the `Tool.Error`'s `error` cause, when it has
    /// one: `(type, message)` with type `permission.rejected`, `tool.execution` or `unknown`.
    pub cause: Option<(String, String)>,
}

/// The `data.cause` of a `tool.error` failure.
fn wire_cause(data: Option<&Value>) -> Option<(String, String)> {
    let cause = data?.get("cause")?;
    Some((cause.get("type")?.as_str()?.to_string(), cause.get("message")?.as_str()?.to_string()))
}

/// An RPC failure `{type, message, data?}`.
#[derive(Debug, Clone, PartialEq, thiserror::Error)]
#[error("{message}")]
pub struct RpcFailure {
    pub kind: String,
    pub message: String,
    pub data: Option<Value>,
}

impl RpcFailure {
    pub fn to_json(&self) -> Value {
        let mut out = json!({ "type": self.kind, "message": self.message });
        if let Some(data) = &self.data {
            out["data"] = data.clone();
        }
        out
    }
}

#[derive(Default)]
struct LocationState {
    reference: Option<LocationRef>,
    request: Option<LoadRequest>,
    in_sidecar: bool,
    running: IndexMap<String, Loaded>,
    hooks: Vec<HookSpec>,
    domains: HashSet<String>,
    revisions: HashMap<String, u64>,
    rpc: Vec<String>,
    inventory: Vec<Value>,
    interest: usize,
}

struct Inner {
    options: HostOptions,
    api: Arc<dyn PluginApi>,
    npm: Npm,
    bun: BunResolver,
    start: tokio::sync::Mutex<()>,
    sidecar: Mutex<Option<Arc<Sidecar>>>,
    locations: Mutex<HashMap<String, LocationState>>,
    progress: Mutex<HashMap<(String, String), ProgressFn>>,
    events: broadcast::Sender<HostEvent>,
    stopping: AtomicBool,
    attempts: Mutex<u32>,
    streams: Arc<Streams>,
}

/// The plugin host. Cheap to clone.
#[derive(Clone)]
pub struct PluginHost {
    inner: Arc<Inner>,
}

const SHUTDOWN_GRACE: Duration = Duration::from_secs(5);

impl PluginHost {
    pub fn new(options: HostOptions, api: Arc<dyn PluginApi>) -> Self {
        let npm = Npm::new(&options.cache, options.locks.clone());
        let bun = BunResolver::new(options.bin.clone());
        let (events, _) = broadcast::channel(256);
        Self {
            inner: Arc::new(Inner {
                options,
                api,
                npm,
                bun,
                start: tokio::sync::Mutex::new(()),
                sidecar: Mutex::new(None),
                locations: Mutex::new(HashMap::new()),
                progress: Mutex::new(HashMap::new()),
                events,
                stopping: AtomicBool::new(false),
                attempts: Mutex::new(0),
                streams: Arc::default(),
            }),
        }
    }

    /// The running sidecar, started if needed.
    pub(crate) async fn sidecar(&self) -> Result<Arc<Sidecar>, String> {
        self.inner.ensure().await
    }

    /// The running sidecar, if any (never starts one).
    pub(crate) fn running(&self) -> Option<Arc<Sidecar>> {
        self.inner.current()
    }

    pub(crate) fn streams(&self) -> Arc<Streams> {
        self.inner.streams.clone()
    }

    pub fn subscribe(&self) -> broadcast::Receiver<HostEvent> {
        self.inner.events.subscribe()
    }

    pub fn npm(&self) -> &Npm {
        &self.inner.npm
    }

    /// Resolves the Bun executable (env override or pinned download).
    pub async fn bun(&self) -> Result<PathBuf, String> {
        self.inner.bun.resolve().await
    }

    /// PID of the running sidecar.
    pub fn sidecar_pid(&self) -> Option<u32> {
        self.inner.current().and_then(|s| s.pid)
    }

    /// Whether the sidecar process is running.
    pub fn is_running(&self) -> bool {
        self.inner.sidecar.lock().as_ref().is_some_and(|s| s.is_alive())
    }

    /// (Re)activates a location's plugins (`PluginSupervisor.activate`). Starts the sidecar only
    /// when an operation needs a package/local plugin. Call again after config or watched-file
    /// changes; unchanged plugins stay active (prefix diff by id and revision).
    pub async fn load(&self, request: LoadRequest) -> LoadResult {
        self.inner.clone().load(request).await
    }

    /// Closes a location: runs plugin cleanups and forgets its registrations.
    pub async fn close_location(&self, location: &LocationRef) {
        let key = location.key();
        let in_sidecar = self.inner.locations.lock().get_mut(&key).map(|s| {
            // Stop replays; keep the reference so cleanups can still call the API.
            s.request = None;
            s.in_sidecar
        });
        if in_sidecar == Some(true)
            && let Some(sidecar) = self.inner.current()
        {
            let _ = sidecar.request("location.close", json!({ "location": key })).await;
        }
        self.inner.locations.lock().remove(&key);
    }

    /// Whether any plugin registered `key` (`<domain>.<name>`) for this provider
    /// (`PluginHooks.has`: unscoped hooks always count).
    pub fn has_hook(&self, location: &LocationRef, key: &str, provider_id: Option<&str>) -> bool {
        self.inner.with(location, |s| {
            s.hooks.iter().any(|h| h.key == key && (h.provider_id.is_none() || h.provider_id.as_deref() == provider_id))
        })
        .unwrap_or(false)
    }

    /// Runs the plugin callbacks for a runtime hook in registration order and returns the mutated
    /// event. Returns `event` untouched (no IPC) when no plugin registered the hook.
    pub async fn trigger_hook(&self, location: &LocationRef, key: &str, event: Value) -> Result<Value, HookError> {
        let registered = self.inner.with(location, |s| s.hooks.iter().any(|h| h.key == key)).unwrap_or(false);
        if !registered {
            return Ok(event);
        }
        let Some(sidecar) = self.inner.current() else { return Ok(event) };
        let mut params = json!({ "location": location.key(), "key": key, "event": event });
        match sidecar.request_with("hook.trigger", &params).await {
            Ok(result) => Ok(result.get("event").cloned().unwrap_or(Value::Null)),
            Err(CallError::Remote(error)) if error.kind.as_deref() == Some("tool.error") => {
                Err(HookError::Tool { cause: wire_cause(error.data.as_ref()), message: error.message, metadata: error.metadata })
            }
            Err(CallError::Remote(error)) if Decline::from_wire(error.kind.as_deref(), error.data.as_ref()).is_some() => {
                Err(HookError::Declined(Decline::from_wire(error.kind.as_deref(), error.data.as_ref()).unwrap()))
            }
            // The sidecar died while the hook ran: like a sidecar that is not running (above), no plugin hook
            // applies. Failing here would end the session's run over a crash that upstream (plugins in-process)
            // cannot have; the restarted sidecar serves the next call.
            Err(CallError::Closed(reason)) => {
                tracing::warn!(hook = key, reason = %reason, "plugin host unavailable; hook skipped");
                Ok(params["event"].take())
            }
            Err(error) => Err(HookError::Failed(error.to_string())),
        }
    }

    /// Domains that currently have plugin transforms.
    pub fn transform_domains(&self, location: &LocationRef) -> Vec<String> {
        self.inner.with(location, |s| s.domains.iter().cloned().collect()).unwrap_or_default()
    }

    /// Increments whenever plugin registrations of `domain` change; compare with a cached value to
    /// know when to rebuild.
    pub fn domain_revision(&self, location: &LocationRef, domain: &str) -> u64 {
        self.inner.with(location, |s| s.revisions.get(domain).copied().unwrap_or(0)).unwrap_or(0)
    }

    /// Applies every plugin transform of `domain` (registration order) to `value`, the domain value
    /// after the native transforms that run before plugins. Returns `value` unchanged (no IPC) when
    /// no plugin transforms the domain. See README "Transform values" for the shapes.
    pub async fn transform(&self, location: &LocationRef, domain: &str, value: Value) -> Result<Value, CallError> {
        Ok(self.transform_ref(location, domain, &value).await?.unwrap_or(value))
    }

    /// [`PluginHost::transform`] of a borrowed value (serialized without a copy); `None` when no
    /// plugin transforms `domain` here (the value is unchanged).
    pub async fn transform_ref(&self, location: &LocationRef, domain: &str, value: &Value) -> Result<Option<Value>, CallError> {
        let active = self.inner.with(location, |s| s.domains.contains(domain)).unwrap_or(false);
        if !active {
            return Ok(None);
        }
        let Some(sidecar) = self.inner.current() else { return Ok(None) };
        #[derive(serde::Serialize)]
        struct Params<'a> {
            location: String,
            domain: &'a str,
            value: &'a Value,
        }
        let mut result = sidecar.request_with("transform", &Params { location: location.key(), domain, value }).await?;
        Ok(Some(result.get_mut("value").map(Value::take).unwrap_or(Value::Null)))
    }

    /// [`PluginHost::transform_ref`] of a large input the sidecar keeps between calls (the catalog
    /// domains): the call names the input by `key` (a fingerprint of the value), and `value` is
    /// built and sent only when the sidecar does not hold that input (first call, changed input,
    /// restarted sidecar).
    pub async fn transform_keyed(
        &self,
        location: &LocationRef,
        domain: &str,
        key: u64,
        value: impl FnOnce() -> Value,
    ) -> Result<Option<Value>, CallError> {
        let active = self.inner.with(location, |s| s.domains.contains(domain)).unwrap_or(false);
        if !active {
            return Ok(None);
        }
        let Some(sidecar) = self.inner.current() else { return Ok(None) };
        let key = format!("{key:016x}");
        #[derive(serde::Serialize)]
        struct Params<'a> {
            location: String,
            domain: &'a str,
            key: &'a str,
            #[serde(skip_serializing_if = "Option::is_none")]
            value: Option<&'a Value>,
        }
        let named = Params { location: location.key(), domain, key: &key, value: None };
        let mut result = match sidecar.request_with("transform", &named).await {
            Ok(result) => result,
            Err(CallError::Remote(error)) if error.kind.as_deref() == Some("transform.input") => {
                let value = value();
                sidecar.request_with("transform", &Params { value: Some(&value), ..named }).await?
            }
            Err(error) => return Err(error),
        };
        Ok(Some(result.get_mut("value").map(Value::take).unwrap_or(Value::Null)))
    }

    /// Executes a plugin tool (`execute` handle of a tool entry with a `js:` handle) with
    /// upstream's runtime semantics: input decoded with the tool's schema, output encoded,
    /// content normalized. `context` is `{sessionID, agent, messageID, id}`; `progress` receives
    /// each `context.progress(metadata)` call. Hooks are not run here (the core runs
    /// `tool.execute.before/after` around it).
    pub async fn execute_tool(
        &self,
        location: &LocationRef,
        handle: &str,
        input: Value,
        context: Value,
        progress: Option<ProgressFn>,
    ) -> Result<Value, ToolError> {
        let sidecar = self.inner.current().ok_or_else(|| ToolError {
            message: "Plugin host is not running".into(),
            metadata: None,
            tool_error: false,
            declined: None,
            cause: None,
        })?;
        let call = context.get("id").and_then(Value::as_str).unwrap_or_default().to_string();
        let key = (location.key(), call);
        if let Some(progress) = progress {
            self.inner.progress.lock().insert(key.clone(), progress);
        }
        let result = sidecar
            .request("tool.execute", json!({ "location": location.key(), "handle": handle, "input": input, "context": context }))
            .await;
        self.inner.progress.lock().remove(&key);
        result.map_err(|error| match error {
            CallError::Remote(e) => ToolError {
                tool_error: e.kind.as_deref() == Some("tool.error"),
                declined: Decline::from_wire(e.kind.as_deref(), e.data.as_ref()),
                cause: wire_cause(e.data.as_ref()).filter(|_| e.kind.as_deref() == Some("tool.error")),
                message: e.message,
                metadata: e.metadata,
            },
            other => ToolError { message: other.to_string(), metadata: None, tool_error: false, declined: None, cause: None },
        })
    }

    /// Calls a plugin function handle (`js:...`) found in a transform value: command `execute`,
    /// websearch `execute`, vcs/worktree operations, integration `authorize`/`refresh`/`label` and
    /// the callbacks they return. Effects returned by the function are run; `scope` keeps their
    /// resources open across calls until [`PluginHost::close_scope`] (used for OAuth attempts).
    pub async fn invoke(&self, location: &LocationRef, handle: &str, args: Vec<Value>, scope: Option<&str>) -> Result<Value, CallError> {
        let sidecar = self.inner.current().ok_or_else(|| CallError::Closed("not started".into()))?;
        let mut params = json!({ "location": location.key(), "handle": handle, "args": args });
        if let Some(scope) = scope {
            params["scope"] = Value::String(scope.into());
        }
        sidecar.request("invoke", params).await
    }

    pub async fn close_scope(&self, location: &LocationRef, scope: &str) {
        if let Some(sidecar) = self.inner.current() {
            let _ = sidecar.request("scope.close", json!({ "location": location.key(), "scope": scope })).await;
        }
    }

    /// RPC ids registered by plugins in this location.
    pub fn rpc_ids(&self, location: &LocationRef) -> Vec<String> {
        self.inner.with(location, |s| s.rpc.clone()).unwrap_or_default()
    }

    /// `POST /api/rpc/:rpcID/:method` for a plugin RPC. Unknown ids fail with
    /// `rpc.unavailable` / `RPC is unavailable: <id>`.
    pub async fn rpc_call(&self, location: &LocationRef, rpc_id: &str, method: &str, input: Value) -> Result<Value, RpcFailure> {
        let unavailable = || RpcFailure { kind: "rpc.unavailable".into(), message: format!("RPC is unavailable: {rpc_id}"), data: None };
        if !self.rpc_ids(location).iter().any(|id| id == rpc_id) {
            return Err(unavailable());
        }
        let Some(sidecar) = self.inner.current() else { return Err(unavailable()) };
        match sidecar.request("rpc.call", json!({ "location": location.key(), "rpcID": rpc_id, "method": method, "input": input })).await {
            Ok(result) => Ok(result.get("output").cloned().unwrap_or(Value::Null)),
            Err(CallError::Remote(error)) => Err(RpcFailure { kind: error.kind.unwrap_or_else(|| "rpc.internal".into()), message: error.message, data: error.data }),
            Err(CallError::Closed(_)) => Err(unavailable()),
        }
    }

    /// Whether any plugin subscribed to `ctx.event.subscribe()` (in any location).
    pub fn wants_events(&self) -> bool {
        self.inner.locations.lock().values().any(|s| s.interest > 0)
    }

    /// Forwards a bus event (wire JSON as sent on `/api/event`) for `ctx.event.subscribe()`. With
    /// a location, only that location's plugins see it. The sidecar filters to server and `rpc.*`
    /// events. No-op when no plugin subscribed.
    pub fn publish_event(&self, location: Option<&LocationRef>, event: &Value) {
        let wanted = match location {
            Some(location) => self.inner.with(location, |s| s.interest > 0).unwrap_or(false),
            None => self.wants_events(),
        };
        if !wanted {
            return;
        }
        if let Some(sidecar) = self.inner.current() {
            sidecar.notify("event", json!({ "location": location.map(|l| l.key()), "event": event }));
        }
    }

    /// Last known plugin inventory of the location's package/local plugins (and failures).
    pub fn packages(&self, location: &LocationRef) -> Vec<Value> {
        self.inner.with(location, |s| s.inventory.clone()).unwrap_or_default()
    }

    /// Closes every location (running plugin cleanups) and stops the sidecar.
    pub async fn shutdown(&self) {
        self.inner.stopping.store(true, Ordering::SeqCst);
        self.inner.locations.lock().clear();
        let sidecar = self.inner.sidecar.lock().take();
        if let Some(sidecar) = sidecar {
            sidecar.shutdown(SHUTDOWN_GRACE).await;
        }
    }
}

impl Inner {
    fn current(&self) -> Option<Arc<Sidecar>> {
        self.sidecar.lock().clone().filter(|s| s.is_alive())
    }

    fn with<T>(&self, location: &LocationRef, f: impl FnOnce(&LocationState) -> T) -> Option<T> {
        self.locations.lock().get(&location.key()).map(f)
    }

    fn reference(&self, key: &str) -> Option<LocationRef> {
        self.locations.lock().get(key).and_then(|s| s.reference.clone())
    }

    fn emit(&self, event: HostEvent) {
        let _ = self.events.send(event);
    }

    fn host_script(&self) -> Result<PathBuf, String> {
        if let Some(path) = std::env::var_os("OCD_PLUGIN_HOST_JS").filter(|v| !v.is_empty()) {
            return Ok(PathBuf::from(path));
        }
        let digest = crate::bun::sha256_hex(HOST_JS.as_bytes());
        let directory = self.options.bin.join("ocd-plugin-host").join(&digest[..16]);
        let path = directory.join("host.js");
        if std::fs::read(&path).is_ok_and(|bytes| crate::bun::sha256_hex(&bytes) == digest) {
            return Ok(path);
        }
        crate::bun::install_bytes(&directory, &path, HOST_JS.as_bytes())?;
        Ok(path)
    }

    async fn ensure(self: &Arc<Self>) -> Result<Arc<Sidecar>, String> {
        if let Some(sidecar) = self.current() {
            return Ok(sidecar);
        }
        let _guard = self.start.lock().await;
        if let Some(sidecar) = self.current() {
            return Ok(sidecar);
        }
        if self.stopping.load(Ordering::SeqCst) {
            return Err("plugin host is shutting down".into());
        }
        let bun = self.bun.resolve().await?;
        let script = self.host_script()?;
        let incoming: Arc<dyn Incoming> = Arc::new(Handler { inner: Arc::downgrade(self) });
        std::fs::create_dir_all(&self.options.cwd).ok();
        let sidecar = Sidecar::spawn(&bun, &script, &self.options.cwd, incoming)
            .map_err(|e| format!("Failed to start plugin host ({}): {e}", bun.display()))?;
        let init = sidecar
            .request("initialize", json!({ "app": self.options.app, "protocol": PROTOCOL_VERSION }))
            .await
            .map_err(|e| format!("Plugin host failed to initialize: {e}"))?;
        let protocol = init.get("protocol").and_then(Value::as_u64).unwrap_or(0);
        if protocol != PROTOCOL_VERSION {
            sidecar.kill();
            return Err(format!("Plugin host protocol mismatch: expected {PROTOCOL_VERSION}, got {protocol}"));
        }
        let version = init.get("bun").and_then(Value::as_str).unwrap_or("?").to_string();
        tracing::info!(pid = ?sidecar.pid, bun = %version, "plugin host started");
        for state in self.locations.lock().values_mut() {
            state.in_sidecar = false;
            state.running.clear();
        }
        *self.sidecar.lock() = Some(sidecar.clone());
        let supervisor = Arc::downgrade(self);
        let watched = sidecar.clone();
        tokio::spawn(async move {
            let started = Instant::now();
            let status = watched.wait().await;
            let Some(inner) = supervisor.upgrade() else { return };
            inner.exited(watched, status, started.elapsed()).await;
        });
        Ok(sidecar)
    }

    // Boxed: `ensure` spawns a task that calls this, which calls `ensure` again.
    fn exited(self: Arc<Self>, sidecar: Arc<Sidecar>, status: String, lived: Duration) -> futures::future::BoxFuture<'static, ()> {
        Box::pin(async move {
        {
            let mut current = self.sidecar.lock();
            if current.as_ref().is_some_and(|c| Arc::ptr_eq(c, &sidecar)) {
                *current = None;
            }
        }
        if self.stopping.load(Ordering::SeqCst) {
            return;
        }
        tracing::error!(%status, "plugin host exited unexpectedly");
        self.emit(HostEvent::Crashed { status });
        let requests: Vec<LoadRequest> = {
            let mut locations = self.locations.lock();
            for state in locations.values_mut() {
                state.in_sidecar = false;
                state.running.clear();
                state.hooks.clear();
                state.rpc.clear();
                state.interest = 0;
                for domain in state.domains.drain() {
                    *state.revisions.entry(domain).or_default() += 1;
                }
            }
            locations.values().filter_map(|s| s.request.clone()).collect()
        };
        if requests.is_empty() {
            return;
        }
        let attempt = {
            let mut attempts = self.attempts.lock();
            if lived > Duration::from_secs(60) {
                *attempts = 0;
            }
            *attempts += 1;
            *attempts
        };
        let delay = Duration::from_secs((1u64 << (attempt - 1).min(5)).min(30));
        tracing::info!(?delay, attempt, "restarting plugin host");
        tokio::time::sleep(delay).await;
        if self.stopping.load(Ordering::SeqCst) {
            return;
        }
        if let Err(error) = self.ensure().await {
            tracing::error!(%error, "failed to restart plugin host");
            return;
        }
        for request in requests {
            self.clone().load(request).await;
        }
        self.emit(HostEvent::Restarted);
            })
    }

    async fn load(self: Arc<Self>, request: LoadRequest) -> LoadResult {
        let key = request.location.key();
        let running = {
            let mut locations = self.locations.lock();
            let state = locations.entry(key.clone()).or_default();
            state.reference = Some(request.location.clone());
            state.request = Some(request.clone());
            state.running.clone()
        };
        let this = self.clone();
        let blocked = request.blocked.clone();
        let resolution = supervisor::resolve(&request.internal, &request.operations, &running, move |operation| {
            let this = this.clone();
            let blocked = blocked.clone();
            let operation = operation.clone();
            async move { this.load_module(&operation, blocked.as_deref()).await }
        })
        .await;

        let packages: Vec<&Loaded> = resolution.package_entries();
        let in_sidecar = self.with(&request.location, |s| s.in_sidecar).unwrap_or(false);
        let mut inventory = resolution.failures.clone();
        if !packages.is_empty() || in_sidecar {
            match self.ensure().await {
                Ok(sidecar) => {
                    let params = json!({
                        "location": key,
                        "ref": request.location,
                        "info": request.info,
                        "plugins": packages.iter().map(|p| json!({ "target": p.target, "revision": p.revision })).collect::<Vec<_>>(),
                        "failures": resolution.failures,
                    });
                    match sidecar.request("location.activate", params).await {
                        Ok(value) => {
                            let activation: Activation = serde_json::from_value(value).unwrap_or_default();
                            inventory = activation.inventory.clone();
                            let mut locations = self.locations.lock();
                            let state = locations.entry(key.clone()).or_default();
                            state.in_sidecar = true;
                            state.hooks = activation.hooks;
                            let domains: HashSet<String> = activation.domains.into_iter().collect();
                            for domain in state.domains.union(&domains) {
                                *state.revisions.entry(domain.clone()).or_default() += 1;
                            }
                            state.domains = domains;
                            state.rpc = activation.rpc;
                        }
                        Err(error) => tracing::error!(%error, location = %request.location.directory, "failed to activate plugins"),
                    }
                }
                Err(error) => tracing::error!(%error, "plugin host unavailable"),
            }
        }
        {
            let mut locations = self.locations.lock();
            let state = locations.entry(key).or_default();
            state.running = resolution.packages.clone();
            state.inventory = inventory.clone();
        }
        self.emit(HostEvent::PluginsChanged { location: request.location.clone() });
        LoadResult { resolution, packages: inventory }
    }

    async fn load_module(self: Arc<Self>, operation: &Operation, blocked: Option<&(dyn Fn(&str) -> bool + Send + Sync)>) -> LoadOutcome {
        let Operation::Add { target, .. } = operation else { return LoadOutcome::Blocked };
        let absolute = Path::new(target).is_absolute();
        let version = target.rfind('@');
        let unversioned = if absolute || version.is_none_or(|v| v == 0) { target.as_str() } else { &target[..version.unwrap_or(0)] };
        if blocked.is_some_and(|blocked| blocked(&format!("plugin:{unversioned}"))) {
            return LoadOutcome::Blocked;
        }
        let failed = |message: &str, cause: String| {
            let r = format!("err_{:08x}", rand::random::<u32>());
            tracing::warn!(target = %target, r#ref = %r, %cause, "failed to load plugin");
            LoadOutcome::Failed { error: message.to_string(), r#ref: Some(r) }
        };
        let sidecar = match self.ensure().await {
            Ok(sidecar) => sidecar,
            Err(error) => return failed("Plugin failed to load", error),
        };
        let install = if absolute {
            None
        } else {
            let (installed, exists) = self.npm.resolve(target);
            if exists {
                Some(installed)
            } else {
                let bun = match self.bun.resolve().await {
                    Ok(bun) => bun,
                    Err(error) => return failed("Plugin failed to load", error),
                };
                match self.npm.add(&bun, target).await {
                    Ok(installed) => Some(installed),
                    Err(error) => return failed("Plugin failed to load", error.to_string()),
                }
            }
        };
        match sidecar.request("module.load", json!({ "operation": operation, "install": install })).await {
            Ok(value) => match serde_json::from_value::<ModuleLoad>(value) {
                Ok(ModuleLoad::Loaded { id, revision, features, source }) => {
                    LoadOutcome::Loaded(Loaded { id, target: target.clone(), revision, source, features })
                }
                Ok(ModuleLoad::Failed { error, r#ref }) => LoadOutcome::Failed { error, r#ref },
                Err(error) => failed("Plugin failed to load", error.to_string()),
            },
            Err(error) => failed("Plugin failed to load", error.to_string()),
        }
    }
}

/// Serves the sidecar's requests and notifications.
struct Handler {
    inner: Weak<Inner>,
}

fn api_error(error: ApiError, rpc: bool) -> WireError {
    WireError { kind: if rpc { error.name.clone() } else { None }, message: error.message, name: error.name, data: error.data, metadata: None }
}

#[async_trait]
impl Incoming for Handler {
    async fn request(&self, method: String, params: Value) -> Result<Value, WireError> {
        let inner = self.inner.upgrade().ok_or_else(|| WireError::new("plugin host is shutting down"))?;
        match method.as_str() {
            "aisdk.http.request" | "aisdk.http.response" => {
                return crate::aisdk::http_call(&inner.streams, &method, &params).await.map_err(WireError::new);
            }
            "npm.add" => {
                // AI SDK provider packages (`loadSDKFactory`): the same npm cache as plugins.
                let spec = params.get("spec").and_then(Value::as_str).unwrap_or_default().to_string();
                let bun = inner.bun.resolve().await.map_err(WireError::new)?;
                let installed = inner.npm.add(&bun, &spec).await.map_err(|e| WireError::new(e.to_string()))?;
                return serde_json::to_value(installed).map_err(|e| WireError::new(e.to_string()));
            }
            _ => {}
        }
        let key = params.get("location").and_then(Value::as_str).unwrap_or_default().to_string();
        let location = inner.reference(&key).ok_or_else(|| WireError::new(format!("Unknown plugin location: {key}")))?;
        match method.as_str() {
            "api" => {
                let name = params.get("method").and_then(Value::as_str).unwrap_or_default().to_string();
                let plugin = params.get("plugin").and_then(Value::as_str).map(String::from);
                let input = params.get("input").cloned().unwrap_or(Value::Null);
                if name.starts_with("storage.") {
                    let plugin = plugin.ok_or_else(|| WireError::new("storage requires a plugin"))?;
                    return crate::storage::handle(inner.api.as_ref(), &plugin, &name, &input).await.map_err(|e| api_error(e, false));
                }
                let rpc = name == "rpc.call";
                inner.api.call(ApiRequest { location, plugin, method: name, input }).await.map_err(|e| api_error(e, rpc))
            }
            "invoke" => {
                let handle = params.get("handle").and_then(Value::as_str).unwrap_or_default();
                let args = params.get("args").and_then(Value::as_array).cloned().unwrap_or_default();
                inner.api.invoke(&location, handle, args).await.map_err(|e| api_error(e, false))
            }
            other => Err(WireError::new(format!("Unknown method: {other}"))),
        }
    }

    fn notification(&self, method: &str, params: Value) {
        let Some(inner) = self.inner.upgrade() else { return };
        if method == "aisdk.part" {
            inner.streams.part(&params);
            return;
        }
        if method == "watch" {
            if let Some(file) = params.get("file").and_then(Value::as_str) {
                inner.emit(HostEvent::Watch { file: PathBuf::from(file) });
            }
            return;
        }
        let key = params.get("location").and_then(Value::as_str).unwrap_or_default().to_string();
        if method == "tool.progress" {
            let call = params.get("id").and_then(Value::as_str).unwrap_or_default().to_string();
            let callback = inner.progress.lock().get(&(key, call)).cloned();
            if let Some(callback) = callback {
                callback(params.get("metadata").cloned().unwrap_or(Value::Null));
            }
            return;
        }
        let event = {
            let mut locations = inner.locations.lock();
            let Some(state) = locations.get_mut(&key) else { return };
            let Some(location) = state.reference.clone() else { return };
            match method {
                "domains.changed" => {
                    let domains: Vec<String> = serde_json::from_value(params.get("domains").cloned().unwrap_or_default()).unwrap_or_default();
                    if let Some(active) = params.get("active").and_then(|a| serde_json::from_value::<HashSet<String>>(a.clone()).ok()) {
                        state.domains = active;
                    }
                    for domain in &domains {
                        *state.revisions.entry(domain.clone()).or_default() += 1;
                    }
                    Some(HostEvent::DomainsChanged { location, domains })
                }
                "hooks.changed" => {
                    state.hooks = serde_json::from_value(params.get("hooks").cloned().unwrap_or_default()).unwrap_or_default();
                    Some(HostEvent::HooksChanged { location })
                }
                "rpc.changed" => {
                    state.rpc = serde_json::from_value(params.get("ids").cloned().unwrap_or_default()).unwrap_or_default();
                    Some(HostEvent::RpcChanged { location })
                }
                "plugins.changed" => {
                    state.inventory = serde_json::from_value(params.get("inventory").cloned().unwrap_or_default()).unwrap_or_default();
                    Some(HostEvent::PluginsChanged { location })
                }
                "events.interest" => {
                    state.interest = params.get("count").and_then(Value::as_u64).unwrap_or(0) as usize;
                    None
                }
                _ => None,
            }
        };
        if let Some(event) = event {
            inner.emit(event);
        }
    }
}
