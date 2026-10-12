//! Daemon process model and IPC client (`persistent-pty/daemon.ts`).
//!
//! One [`DaemonTransport`] per server process. It lazily spawns
//! `opencode-pty daemon --runtime-dir <root> --name <id>`, discovers it through
//! `<root>/<id>/service.json`, keeps one long-lived authenticated *owner* connection
//! (`own`), sends one-shot framed requests on fresh connections, opens dedicated
//! subscription connections, and can prepare / adopt a restart handoff.

use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Weak};
use std::time::{Duration, Instant};

use futures::future::BoxFuture;
use parking_lot::Mutex;
use serde_json::{Value, json};
use tokio::io::{AsyncRead, AsyncWrite, AsyncWriteExt};
use tokio::sync::{Notify, mpsc, oneshot};

use crate::frame::{FrameReader, encode_envelope, parse_output_frame};
use crate::handoff::Handoff;
use crate::wire::{Registration, Role, StreamEvent, WireResponse, WireTerminal, decode_base64, stream_event};

const TIMEOUT: Duration = Duration::from_secs(5);
const POLL: Duration = Duration::from_millis(50);

/// `DaemonError.kind`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ErrorKind {
    Connect,
    Response,
    Registration,
    Protocol,
    Spawn,
}

/// `PersistentPty.DaemonError`.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{message}")]
pub struct DaemonError {
    pub kind: ErrorKind,
    pub message: String,
    pub pid: Option<u64>,
}

impl DaemonError {
    pub fn new(kind: ErrorKind, message: impl Into<String>) -> Self {
        Self { kind, message: message.into(), pid: None }
    }
}

fn failure(kind: ErrorKind) -> impl Fn(String) -> DaemonError {
    move |message| DaemonError::new(kind, message)
}

/// Async binary resolver (`() => Promise<string>`).
pub type BinaryFn = Arc<dyn Fn() -> BoxFuture<'static, Result<String, String>> + Send + Sync>;

/// Called for every stream event before it is delivered (used for auto-removal on exit).
pub type EventHook = Arc<dyn Fn(&StreamEvent) + Send + Sync>;

#[cfg(unix)]
type Stream = tokio::net::UnixStream;
#[cfg(unix)]
async fn connect_socket(path: &str) -> std::io::Result<Stream> {
    tokio::net::UnixStream::connect(path).await
}
#[cfg(not(unix))]
type Stream = tokio::io::DuplexStream;
#[cfg(not(unix))]
async fn connect_socket(_path: &str) -> std::io::Result<Stream> {
    Err(std::io::Error::new(std::io::ErrorKind::Unsupported, "Persistent PTYs are unavailable on this platform"))
}

/// Replay data returned by `subscribe`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Replay {
    pub requested_offset: u64,
    pub available_offset: u64,
    pub end_offset: u64,
    pub truncated: bool,
    pub data: Vec<u8>,
}

/// Cloneable handle that destroys a subscription socket (`detach()`).
#[derive(Clone, Default)]
pub struct DetachHandle(Arc<DetachState>);

#[derive(Default)]
struct DetachState {
    detached: AtomicBool,
    notify: Notify,
}

impl DetachHandle {
    pub fn detach(&self) {
        if !self.0.detached.swap(true, Ordering::AcqRel) {
            self.0.notify.notify_one();
        }
    }

    pub fn is_detached(&self) -> bool {
        self.0.detached.load(Ordering::Acquire)
    }
}

/// `DaemonAttachment`: the `attached` response plus a not-yet-started event pump.
pub struct DaemonAttachment {
    pub terminal: WireTerminal,
    pub role: Role,
    pub generation: u64,
    pub replay: Replay,
    reader: Option<FrameReader<Stream>>,
    handle: DetachHandle,
}

impl DaemonAttachment {
    /// `activate()`: start the pump. Returns `None` if already activated or detached.
    /// The receiver yields live events and closes when the stream ends (after `exited`,
    /// on EOF or error) or after `detach()`.
    pub fn activate(&mut self, hook: Option<EventHook>) -> Option<mpsc::UnboundedReceiver<StreamEvent>> {
        if self.handle.is_detached() {
            return None;
        }
        let reader = self.reader.take()?;
        let (tx, rx) = mpsc::unbounded_channel();
        tokio::spawn(pump(reader, self.handle.clone(), hook, tx));
        Some(rx)
    }

    pub fn detach(&self) {
        self.handle.detach();
        // An unactivated socket is closed when `self` drops; activated pumps watch the handle.
    }

    pub fn detach_handle(&self) -> DetachHandle {
        self.handle.clone()
    }
}

async fn pump(
    mut reader: FrameReader<Stream>,
    handle: DetachHandle,
    hook: Option<EventHook>,
    tx: mpsc::UnboundedSender<StreamEvent>,
) {
    loop {
        if handle.is_detached() {
            break;
        }
        let frame = tokio::select! {
            _ = handle.0.notify.notified() => break,
            frame = reader.next() => frame,
        };
        let Ok(Some(frame)) = frame else { break };
        let event = match parse_output_frame(&frame) {
            Some(Ok(output)) => StreamEvent::Output { start: output.start, end: output.end, data: output.data.to_vec() },
            Some(Err(_)) => break,
            None => match WireResponse::decode(&frame).and_then(stream_event) {
                Ok(Some(event)) => event,
                Ok(None) => continue,
                Err(_) => break,
            },
        };
        let exited = matches!(event, StreamEvent::Exited { .. });
        if let Some(hook) = &hook {
            hook(&event);
        }
        if tx.send(event).is_err() || exited {
            break;
        }
    }
}

struct OwnerCall {
    request: Value,
    reply: oneshot::Sender<Result<WireResponse, String>>,
}

struct Owner {
    seq: u64,
    tx: mpsc::Sender<OwnerCall>,
}

struct State {
    id: String,
    registration: Option<Registration>,
    owner: Option<Owner>,
    closed: bool,
    seq: u64,
}

/// `DaemonTransport`.
pub struct DaemonTransport {
    root: PathBuf,
    binary: BinaryFn,
    state: Arc<Mutex<State>>,
    startup: tokio::sync::Mutex<()>,
}

impl Drop for DaemonTransport {
    fn drop(&mut self) {
        self.close();
    }
}

impl DaemonTransport {
    /// `makeDaemonTransport(root, initialID, binary, inherited)`. Adoption failures of an
    /// inherited handoff are logged and abandoned; construction itself never fails.
    pub async fn new(root: PathBuf, initial_id: String, binary: BinaryFn, inherited: Option<Handoff>) -> Self {
        let transport = Self {
            root,
            binary,
            state: Arc::new(Mutex::new(State { id: initial_id, registration: None, owner: None, closed: false, seq: 0 })),
            startup: tokio::sync::Mutex::new(()),
        };
        if let Some(inherited) = inherited {
            transport.adopt(inherited).await;
        }
        transport
    }

    /// Finalizer: destroy the owner socket. Terminals are not terminated by the server, but the
    /// daemon stops when its owner disconnects without a prepared handoff.
    pub fn close(&self) {
        let mut state = self.state.lock();
        state.closed = true;
        state.owner = None;
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Current daemon name (`<root>/<id>`).
    pub fn id(&self) -> String {
        self.state.lock().id.clone()
    }

    /// Runtime directory `<root>/<id>`.
    pub fn directory(&self) -> PathBuf {
        self.root.join(self.id())
    }

    pub fn is_owner(&self) -> bool {
        self.state.lock().owner.is_some()
    }

    async fn adopt(&self, inherited: Handoff) {
        let mut found: Option<Registration> = None;
        let result: Result<(), DaemonError> = async {
            if (inherited.expires_at as i128) <= now_ms() as i128 {
                return Err(DaemonError::new(ErrorKind::Registration, "PTY restart handoff expired"));
            }
            // Older releases put the daemon directory outside the root; its directory name is still the ID.
            let directory = PathBuf::from(&inherited.directory);
            let id = directory.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
            self.state.lock().id = id.clone();
            let registration = self.discover(Some(&directory)).await?;
            found = Some(registration.clone());
            if registration.instance_id != inherited.instance_id {
                return Err(DaemonError::new(ErrorKind::Registration, "PTY restart daemon changed"));
            }
            let target = self.root.join(&id);
            if normalize(&absolute(&directory)) != normalize(&absolute(&target)) {
                migrate(&directory, &target).await?;
            }
            self.claim(&registration, Some(&inherited.ticket)).await
        }
        .await;
        if let Err(error) = result {
            tracing::warn!(cause = %error.message, "Discarding PTY restart handoff; persistent terminals will start fresh");
            if let Some(found) = found {
                let _ = one_shot(&found, json!({ "op": "shutdown" })).await;
            }
            self.abandon();
        }
    }

    async fn claim(&self, current: &Registration, ticket: Option<&str>) -> Result<(), DaemonError> {
        let opened = tokio::time::timeout(TIMEOUT, open_owner(current, ticket))
            .await
            .map_err(|_| DaemonError::new(ErrorKind::Connect, "PTY ownership claim timed out"))?
            .map_err(failure(ErrorKind::Connect))?;
        let (reader, writer) = opened;
        let mut state = self.state.lock();
        if state.closed {
            return Err(DaemonError::new(ErrorKind::Connect, "PTY owner scope is closed"));
        }
        state.seq += 1;
        let seq = state.seq;
        let (tx, rx) = mpsc::channel(4);
        state.owner = Some(Owner { seq, tx });
        state.registration = Some(current.clone());
        tokio::spawn(owner_actor(Arc::downgrade(&self.state), seq, current.token.clone(), reader, writer, rx));
        Ok(())
    }

    /// `discover(directory = <root>/<id>)`.
    pub async fn discover(&self, directory: Option<&Path>) -> Result<Registration, DaemonError> {
        let directory = directory.map(Path::to_path_buf).unwrap_or_else(|| self.directory());
        discover_in(&directory).await
    }

    async fn spawn_daemon(&self) -> Result<Registration, DaemonError> {
        let executable = (self.binary)().await.map_err(failure(ErrorKind::Spawn))?;
        let id = self.id();
        let mut command = tokio::process::Command::new(&executable);
        command
            .arg("daemon")
            .arg("--runtime-dir")
            .arg(&self.root)
            .arg("--name")
            .arg(&id)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null());
        #[cfg(unix)]
        // SAFETY: setsid is async-signal-safe; this mirrors Node's `detached: true`.
        unsafe {
            command.pre_exec(|| {
                libc::setsid();
                Ok(())
            });
        }
        let mut child = command.spawn().map_err(|e| DaemonError::new(ErrorKind::Spawn, e.to_string()))?;
        let result = async {
            let deadline = Instant::now() + TIMEOUT;
            let mut last: Option<DaemonError> = None;
            while Instant::now() < deadline && matches!(child.try_wait(), Ok(None)) {
                match self.discover(None).await {
                    Ok(found) => {
                        self.claim(&found, None).await?;
                        return Ok(found);
                    }
                    Err(error) => last = Some(error),
                }
                tokio::time::sleep(POLL).await;
            }
            Err(last.unwrap_or_else(|| DaemonError::new(ErrorKind::Connect, "opencode-pty did not become ready")))
        }
        .await;
        if result.is_err() {
            terminate(&mut child);
        }
        result
    }

    /// An unreachable daemon can still hold this directory's lock, so retry once in a fresh directory.
    async fn start(&self) -> Result<Registration, DaemonError> {
        match self.spawn_daemon().await {
            Ok(found) => Ok(found),
            Err(error) => {
                tracing::warn!(cause = %error.message, "Starting opencode-pty in a fresh runtime directory");
                self.abandon();
                self.spawn_daemon().await
            }
        }
    }

    fn abandon(&self) {
        let mut state = self.state.lock();
        state.owner = None;
        state.registration = None;
        state.id = uuid::Uuid::new_v4().to_string();
    }

    async fn connect(&self, should_start: bool) -> Result<Registration, DaemonError> {
        {
            let state = self.state.lock();
            if state.closed {
                return Err(DaemonError::new(ErrorKind::Connect, "PTY owner scope is closed"));
            }
            if let Some(registration) = &state.registration {
                return Ok(registration.clone());
            }
        }
        let _permit = self.startup.lock().await;
        if let Some(registration) = &self.state.lock().registration {
            return Ok(registration.clone());
        }
        let found = match self.discover(None).await {
            Ok(found) => found,
            Err(error) if should_start && error.kind == ErrorKind::Connect => self.start().await?,
            Err(error) => return Err(error),
        };
        let owned = self.state.lock().owner.is_some();
        if !owned {
            self.claim(&found, None).await?;
        }
        self.state.lock().registration = Some(found.clone());
        Ok(found)
    }

    fn forget(&self, current: &Registration) {
        let mut state = self.state.lock();
        if state.registration.as_ref() == Some(current) {
            state.registration = None;
        }
    }

    async fn attempt(&self, value: &Value, should_start: bool) -> Result<WireResponse, DaemonError> {
        let current = self.connect(should_start).await?;
        one_shot(&current, value.clone()).await.inspect_err(|error| {
            if error.kind == ErrorKind::Registration {
                self.forget(&current);
            }
        })
    }

    /// `request(value, start)`: one framed request with the upstream retry policy.
    pub async fn request(&self, value: Value, should_start: bool) -> Result<WireResponse, DaemonError> {
        match self.attempt(&value, should_start).await {
            Ok(response) => Ok(response),
            Err(error) if error.kind == ErrorKind::Registration => self.attempt(&value, should_start).await,
            Err(error) if error.kind != ErrorKind::Connect => Err(error),
            Err(error) => {
                self.state.lock().registration = None;
                if !should_start {
                    return Err(error);
                }
                self.attempt(&value, true).await
            }
        }
    }

    /// `requestIfRunning(value)`: `Ok(None)` when no daemon is reachable.
    pub async fn request_if_running(&self, value: Value) -> Result<Option<WireResponse>, DaemonError> {
        match self.request(value, false).await {
            Ok(response) => Ok(Some(response)),
            Err(error) if error.kind == ErrorKind::Connect => Ok(None),
            Err(error) => Err(error),
        }
    }

    /// `shutdown`: ask the daemon to stop and wait (≤ 5 s) until it is gone.
    pub async fn shutdown(&self) -> Result<Option<WireResponse>, DaemonError> {
        let response = self.request_if_running(json!({ "op": "shutdown" })).await?;
        self.state.lock().registration = None;
        let Some(response) = response else { return Ok(None) };
        let deadline = Instant::now() + TIMEOUT;
        while Instant::now() < deadline {
            if self.discover(None).await.is_err() {
                return Ok(Some(response));
            }
            tokio::time::sleep(POLL).await;
        }
        Err(DaemonError::new(ErrorKind::Connect, "opencode-pty did not stop"))
    }

    /// `handoff`: `prepare_handoff` over the owner connection. `Ok(None)` when not owning a daemon.
    pub async fn handoff(&self) -> Result<Option<Handoff>, DaemonError> {
        let _permit = self.startup.lock().await;
        let (tx, registered, id) = {
            let state = self.state.lock();
            match (&state.owner, &state.registration) {
                (Some(owner), Some(registration)) => (owner.tx.clone(), registration.clone(), state.id.clone()),
                _ => return Ok(None),
            }
        };
        let (reply, receive) = oneshot::channel();
        let closed = || DaemonError::new(ErrorKind::Response, "PTY daemon closed its ownership connection");
        tx.send(OwnerCall { request: json!({ "op": "prepare_handoff" }), reply }).await.map_err(|_| closed())?;
        let response = receive.await.map_err(|_| closed())?.map_err(failure(ErrorKind::Response))?;
        let WireResponse::Handoff { ticket, expires_at } = response else {
            return Err(DaemonError::new(ErrorKind::Protocol, "Expected PTY handoff ticket"));
        };
        Ok(Some(Handoff {
            directory: self.root.join(id).to_string_lossy().into_owned(),
            instance_id: registered.instance_id,
            ticket,
            expires_at: expires_at as i64,
        }))
    }

    /// `subscribe(id, input)`.
    pub async fn subscribe(&self, terminal: u64, input: &SubscribeInput) -> Result<DaemonAttachment, DaemonError> {
        match self.subscribe_attempt(terminal, input).await {
            Err(error) if error.kind == ErrorKind::Registration => self.subscribe_attempt(terminal, input).await,
            other => other,
        }
    }

    async fn subscribe_attempt(&self, terminal: u64, input: &SubscribeInput) -> Result<DaemonAttachment, DaemonError> {
        let current = self.connect(false).await?;
        subscribe_raw(&current, terminal, input).await.inspect_err(|error| {
            if error.kind == ErrorKind::Registration {
                self.forget(&current);
            }
        })
    }
}

/// Subscription parameters.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SubscribeInput {
    pub cursor: u64,
    pub attachment_id: String,
    pub role: Role,
    pub takeover: bool,
}

fn terminate(child: &mut tokio::process::Child) {
    #[cfg(unix)]
    if let Some(pid) = child.id() {
        // SAFETY: plain kill(2) on our own child's pid.
        unsafe {
            libc::kill(pid as libc::pid_t, libc::SIGTERM);
        }
        return;
    }
    let _ = child.start_kill();
}

pub(crate) fn now_ms() -> i64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_millis() as i64).unwrap_or(0)
}

fn absolute(path: &Path) -> PathBuf {
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir().unwrap_or_else(|_| PathBuf::from("/")).join(path)
    }
}

/// Lexical normalization (`path.resolve`).
fn normalize(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            Component::ParentDir => {
                out.pop();
            }
            Component::CurDir => {}
            other => out.push(other.as_os_str()),
        }
    }
    out
}

/// Older releases kept registrations in purgeable temp directories. Renaming keeps the lock file's
/// inode, so the still-running daemon keeps holding its lock in the new location.
async fn migrate(directory: &Path, target: &Path) -> Result<(), DaemonError> {
    let directory = directory.to_path_buf();
    let target = target.to_path_buf();
    tokio::task::spawn_blocking(move || -> std::io::Result<()> {
        let mut builder = std::fs::DirBuilder::new();
        builder.recursive(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::DirBuilderExt;
            builder.mode(0o700);
        }
        builder.create(&target)?;
        std::fs::rename(directory.join("service.lock"), target.join("service.lock"))?;
        std::fs::rename(directory.join("service.json"), target.join("service.json"))?;
        Ok(())
    })
    .await
    .map_err(|e| DaemonError::new(ErrorKind::Registration, e.to_string()))?
    .map_err(|e| DaemonError::new(ErrorKind::Registration, e.to_string()))
}

pub(crate) async fn discover_in(directory: &Path) -> Result<Registration, DaemonError> {
    let text = tokio::fs::read_to_string(directory.join("service.json"))
        .await
        .map_err(|e| DaemonError::new(ErrorKind::Connect, e.to_string()))?;
    let decoded: Registration =
        serde_json::from_str(&text).map_err(|e| DaemonError::new(ErrorKind::Protocol, e.to_string()))?;
    if decoded.protocol != crate::frame::PROTOCOL_VERSION {
        return Err(DaemonError {
            kind: ErrorKind::Protocol,
            message: format!(
                "opencode-pty protocol mismatch: daemon={}, client={}",
                decoded.protocol,
                crate::frame::PROTOCOL_VERSION
            ),
            pid: Some(decoded.pid),
        });
    }
    let response = one_shot(&decoded, json!({ "op": "ping" })).await?;
    match response {
        WireResponse::Pong { instance_id, pid, protocol }
            if instance_id == decoded.instance_id && pid == decoded.pid && protocol == crate::frame::PROTOCOL_VERSION =>
        {
            Ok(decoded)
        }
        _ => Err(DaemonError::new(ErrorKind::Protocol, "opencode-pty registration mismatch")),
    }
}

fn error_kind(message: &str) -> ErrorKind {
    if message == "authentication failed" { ErrorKind::Registration } else { ErrorKind::Protocol }
}

/// `oneShot(registration, request)`: a fresh connection per request, 5 s overall timeout.
pub(crate) async fn one_shot(registration: &Registration, request: Value) -> Result<WireResponse, DaemonError> {
    let payload = encode_envelope(&registration.token, &request).map_err(failure(ErrorKind::Protocol))?;
    let dispatched = AtomicBool::new(false);
    let exchange = async {
        let mut stream = connect_socket(&registration.socket)
            .await
            .map_err(|e| DaemonError::new(ErrorKind::Connect, e.to_string()))?;
        dispatched.store(true, Ordering::Release);
        stream.write_all(&payload).await.map_err(|e| DaemonError::new(ErrorKind::Response, e.to_string()))?;
        let mut reader = FrameReader::new(&mut stream);
        let frame = reader
            .next()
            .await
            .map_err(failure(ErrorKind::Response))?
            .ok_or_else(|| DaemonError::new(ErrorKind::Response, "opencode-pty closed without response"))?;
        let response = WireResponse::decode(&frame).map_err(failure(ErrorKind::Protocol))?;
        if let WireResponse::Error { message } = response {
            return Err(DaemonError::new(error_kind(&message), message));
        }
        Ok(response)
    };
    match tokio::time::timeout(TIMEOUT, exchange).await {
        Ok(result) => result,
        Err(_) => Err(DaemonError::new(
            if dispatched.load(Ordering::Acquire) { ErrorKind::Response } else { ErrorKind::Connect },
            "opencode-pty request timed out",
        )),
    }
}

type OwnerReader = FrameReader<tokio::io::ReadHalf<Stream>>;
type OwnerWriter = tokio::io::WriteHalf<Stream>;

async fn exchange<R: AsyncRead + Unpin, W: AsyncWrite + Unpin>(
    reader: &mut FrameReader<R>,
    writer: &mut W,
    token: &str,
    request: &Value,
) -> Result<WireResponse, String> {
    let payload = encode_envelope(token, request)?;
    writer.write_all(&payload).await.map_err(|e| e.to_string())?;
    let frame = reader.next().await?.ok_or_else(|| "PTY daemon closed its ownership connection".to_string())?;
    let response = WireResponse::decode(&frame)?;
    if let WireResponse::Error { message } = response {
        return Err(message);
    }
    Ok(response)
}

/// `openOwner`: connect and claim ownership (`own`).
async fn open_owner(registration: &Registration, ticket: Option<&str>) -> Result<(OwnerReader, OwnerWriter), String> {
    let stream = connect_socket(&registration.socket).await.map_err(|e| e.to_string())?;
    let (read, mut write) = tokio::io::split(stream);
    let mut reader = FrameReader::new(read);
    let mut request = json!({ "op": "own", "instance_id": registration.instance_id });
    if let Some(ticket) = ticket {
        request["ticket"] = Value::String(ticket.to_string());
    }
    let response = exchange(&mut reader, &mut write, &registration.token, &request).await?;
    if response != WireResponse::Owned {
        return Err("PTY daemon does not support server ownership".into());
    }
    Ok((reader, write))
}

/// Owns the owner socket: serializes exchanges and notices when the daemon closes it.
async fn owner_actor(
    state: Weak<Mutex<State>>,
    seq: u64,
    token: String,
    mut reader: OwnerReader,
    mut writer: OwnerWriter,
    mut calls: mpsc::Receiver<OwnerCall>,
) {
    loop {
        tokio::select! {
            call = calls.recv() => {
                let Some(call) = call else { break };
                match tokio::time::timeout(TIMEOUT, exchange(&mut reader, &mut writer, &token, &call.request)).await {
                    Ok(result) => {
                        let _ = call.reply.send(result);
                    }
                    Err(_) => {
                        let _ = call.reply.send(Err("PTY handoff preparation timed out".into()));
                        break;
                    }
                }
            }
            frame = reader.next() => {
                // The daemon never writes unsolicited frames; EOF/error means the connection is gone.
                if !matches!(frame, Ok(Some(_))) {
                    break;
                }
            }
        }
    }
    drop(writer);
    drop(reader);
    if let Some(state) = state.upgrade() {
        let mut state = state.lock();
        if state.owner.as_ref().is_some_and(|owner| owner.seq == seq) {
            state.owner = None;
            state.registration = None;
        }
    }
}

/// `subscribePromise`.
async fn subscribe_raw(
    registration: &Registration,
    terminal: u64,
    input: &SubscribeInput,
) -> Result<DaemonAttachment, DaemonError> {
    let connect = |message: String| DaemonError::new(ErrorKind::Connect, message);
    let mut stream = connect_socket(&registration.socket).await.map_err(|e| connect(e.to_string()))?;
    let payload = encode_envelope(
        &registration.token,
        &json!({
            "op": "subscribe",
            "id": terminal,
            "offset": input.cursor,
            "attachment_id": input.attachment_id,
            "role": input.role.as_str(),
            "takeover": input.takeover,
        }),
    )
    .map_err(connect)?;
    stream.write_all(&payload).await.map_err(|e| connect(e.to_string()))?;
    let mut reader = FrameReader::new(stream);
    let initial = reader.next().await.map_err(connect)?.ok_or_else(|| connect("opencode-pty closed before attachment".into()))?;
    let response = WireResponse::decode(&initial).map_err(connect)?;
    match response {
        WireResponse::Error { message } => Err(DaemonError::new(error_kind(&message), message)),
        WireResponse::Attached {
            terminal,
            role,
            generation,
            requested_offset,
            available_offset,
            end_offset,
            truncated,
            replay_base64,
        } => Ok(DaemonAttachment {
            terminal,
            role,
            generation,
            replay: Replay { requested_offset, available_offset, end_offset, truncated, data: decode_base64(&replay_base64) },
            reader: Some(reader),
            handle: DetachHandle::default(),
        }),
        other => Err(connect(format!("unexpected opencode-pty response: {}", other.type_name()))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalizes_paths() {
        assert_eq!(normalize(Path::new("/a/./b/../c")), PathBuf::from("/a/c"));
        assert_eq!(normalize(Path::new("/a/b/")), PathBuf::from("/a/b"));
    }

    #[test]
    fn auth_failures_are_registration_errors() {
        assert_eq!(error_kind("authentication failed"), ErrorKind::Registration);
        assert_eq!(error_kind("terminal 3 not found"), ErrorKind::Protocol);
    }
}
