//! Client for opencode's persistent terminals (`packages/core/src/persistent-pty/**`).
//!
//! Terminals live in the external `opencode-pty` daemon (npm `@opencode-ai/pty@0.2.0`,
//! protocol 7) so they survive server restarts. [`PersistentPty`] is the service the HTTP
//! `persistentPty.*` endpoints and the TUI terminal pane use; see `README.md`.

pub mod binary;
pub mod daemon;
pub mod frame;
pub mod handoff;
pub mod types;
pub mod wire;
pub mod ws;

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::{Arc, Weak};
use std::task::{Context, Poll};

use parking_lot::Mutex;
use serde_json::{Value, json};
use tokio::sync::{broadcast, mpsc};

pub use binary::{AVAILABLE, BinaryResolver, available};
pub use daemon::{BinaryFn, DaemonError, DaemonTransport, DetachHandle, ErrorKind, Replay, SubscribeInput};
pub use handoff::Handoff;
pub use types::{
    BusEvent, CreateInput, Cursor, Info, Output, ReadResult, Screen, Size, Snapshot, Status, UpdateInput, from_id, to_id,
};
pub use wire::{Role, StreamEvent};

/// Service errors (`PersistentPty.UnavailableError` / `PersistentPty.NotFoundError`).
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum Error {
    /// Maps to HTTP `ServiceUnavailableError {message, service: "opencode-pty"}`.
    #[error("{0}")]
    Unavailable(String),
    /// Maps to HTTP `PtyNotFoundError {ptyID, message: "PTY session not found: <id>"}`.
    #[error("PTY session not found: {0}")]
    NotFound(String),
}

pub type Result<T, E = Error> = std::result::Result<T, E>;

impl From<DaemonError> for Error {
    fn from(error: DaemonError) -> Self {
        Error::Unavailable(error.message)
    }
}

fn unexpected(response: &wire::WireResponse) -> Error {
    Error::Unavailable(format!("unexpected opencode-pty response: {}", response.type_name()))
}

/// Default program for new terminals (`ShellSelect.environment(global.bin)`).
pub type ShellFn = Arc<dyn Fn() -> String + Send + Sync>;

/// `PersistentPty.Options` plus Rust-side injection points.
#[derive(Clone, Default)]
pub struct Options {
    /// Inherited restart handoff (see [`handoff::take_from_env`]).
    pub handoff: Option<Handoff>,
    /// Parent of per-server daemon runtime directories; defaults to `<state>/pty`.
    pub root: Option<PathBuf>,
    /// Override the binary resolver (default: [`BinaryResolver`] over `global.bin`).
    pub binary: Option<BinaryFn>,
    /// Override the default shell (default: [`shell_environment`]).
    pub shell: Option<ShellFn>,
    /// HTTP client for the npm download (default: a new client).
    pub http: Option<reqwest::Client>,
    /// Initial daemon name (default: random UUID).
    pub id: Option<String>,
}

/// Attach parameters (`persistentPty.connect` query).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AttachInput {
    pub cursor: u64,
    pub attachment_id: String,
    pub role: Role,
    pub takeover: bool,
}

/// A live attachment (`PersistentPty.Attachment`).
pub struct Attachment {
    pub info: Info,
    pub role: Role,
    pub generation: u64,
    pub replay: Replay,
    inner: daemon::DaemonAttachment,
    hook: daemon::EventHook,
}

impl Attachment {
    /// Start delivering live events (`activate()`). `None` if already activated or detached.
    /// The stream ends after `exited`, on daemon EOF/error, or after [`Attachment::detach`].
    pub fn activate(&mut self) -> Option<EventStream> {
        self.inner.activate(Some(self.hook.clone())).map(|rx| EventStream { rx })
    }

    /// Destroy the subscription socket (`detach()`).
    pub fn detach(&self) {
        self.inner.detach();
    }

    pub fn detach_handle(&self) -> DetachHandle {
        self.inner.detach_handle()
    }
}

/// Live events of an activated attachment.
pub struct EventStream {
    rx: mpsc::UnboundedReceiver<StreamEvent>,
}

impl EventStream {
    pub async fn recv(&mut self) -> Option<StreamEvent> {
        self.rx.recv().await
    }
}

impl futures::Stream for EventStream {
    type Item = StreamEvent;
    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<StreamEvent>> {
        self.rx.poll_recv(cx)
    }
}

struct Inner {
    daemon: DaemonTransport,
    events: broadcast::Sender<BusEvent>,
    /// Controller activity selects a terminal; observer reads and pane visibility do not.
    current: Mutex<HashMap<String, String>>,
    removing: Mutex<HashSet<String>>,
    shell: ShellFn,
}

/// The persistent PTY service (`PersistentPty.Service`). Cheap to clone.
#[derive(Clone)]
pub struct PersistentPty {
    inner: Arc<Inner>,
}

impl PersistentPty {
    /// `makeLayer(options)`. Adopts `options.handoff` when given (failures are logged and the
    /// old daemon is abandoned). Never fails; the daemon is spawned lazily on first `create`.
    pub async fn new(global: &oc_util::Global, options: Options) -> Self {
        let root = options.root.clone().unwrap_or_else(|| global.state.join("pty"));
        let binary = options.binary.clone().unwrap_or_else(|| {
            let resolver = BinaryResolver::new(global.bin.clone(), options.http.clone().unwrap_or_default());
            memoized_binary(resolver)
        });
        let shell = options.shell.clone().unwrap_or_else(|| {
            let bin = global.bin.clone();
            Arc::new(move || shell_environment(&bin))
        });
        let id = options.id.clone().unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
        let daemon = DaemonTransport::new(root, id, binary, options.handoff.clone()).await;
        let (events, _) = broadcast::channel(256);
        PersistentPty {
            inner: Arc::new(Inner {
                daemon,
                events,
                current: Mutex::new(HashMap::new()),
                removing: Mutex::new(HashSet::new()),
                shell,
            }),
        }
    }

    /// `available` (false on Windows; reported as `capabilities.persistentPty`).
    pub fn available(&self) -> bool {
        AVAILABLE
    }

    /// `persistent-pty.added` / `persistent-pty.removed` events.
    pub fn subscribe_events(&self) -> broadcast::Receiver<BusEvent> {
        self.inner.events.subscribe()
    }

    /// The underlying transport (runtime directory, ownership state).
    pub fn daemon(&self) -> &DaemonTransport {
        &self.inner.daemon
    }

    /// Scope finalizer: drop the owner connection (the daemon stops unless a handoff was prepared).
    pub fn close(&self) {
        self.inner.daemon.close();
    }

    fn publish(&self, event: BusEvent) {
        let _ = self.inner.events.send(event);
    }

    async fn request(&self, value: Value, start: bool) -> Result<wire::WireResponse> {
        Ok(self.inner.daemon.request(value, start).await?)
    }

    /// `list(sessionID?)`: `[]` when no daemon is running.
    pub async fn list(&self, session_id: Option<&str>) -> Result<Vec<Info>> {
        let Some(response) = self.inner.daemon.request_if_running(json!({ "op": "list" })).await? else {
            return Ok(Vec::new());
        };
        let wire::WireResponse::Terminals { terminals } = &response else { return Err(unexpected(&response)) };
        Ok(terminals
            .iter()
            .map(Info::from_wire)
            .filter(|t| session_id.is_none_or(|s| t.session_id == s))
            .collect())
    }

    /// `get(id)`.
    pub async fn get(&self, id: &str) -> Result<Info> {
        self.list(None).await?.into_iter().find(|t| t.id == id).ok_or_else(|| Error::NotFound(id.to_string()))
    }

    fn wire_id(id: &str) -> Result<u64> {
        from_id(id).map_err(Error::Unavailable)
    }

    /// `create(sessionID, input)`: spawns the daemon on first use; publishes `persistent-pty.added`.
    pub async fn create(&self, session_id: &str, input: CreateInput) -> Result<Info> {
        let program = input.command.clone().unwrap_or_else(|| (self.inner.shell)());
        let response = self
            .request(
                json!({
                    "op": "create",
                    "program": program,
                    "args": input.args,
                    "cwd": input.cwd.clone().unwrap_or_else(|| "/".to_string()),
                    "title": input.title,
                    "group_id": session_id,
                    "env": input.env,
                    "cols": input.size.map(|s| s.cols).unwrap_or(80),
                    "rows": input.size.map(|s| s.rows).unwrap_or(24),
                }),
                true,
            )
            .await?;
        let wire::WireResponse::Created { terminal } = &response else { return Err(unexpected(&response)) };
        let terminal = Info::from_wire(terminal);
        self.publish(BusEvent::Added { session_id: session_id.to_string(), terminal: terminal.clone() });
        Ok(terminal)
    }

    fn expect_ok(response: wire::WireResponse) -> Result<()> {
        if response == wire::WireResponse::Ok { Ok(()) } else { Err(unexpected(&response)) }
    }

    /// `write(id, data, attachmentID?)`.
    pub async fn write(&self, id: &str, data: &[u8], attachment_id: Option<&str>) -> Result<()> {
        self.get(id).await?;
        let response = self
            .request(
                json!({
                    "op": "write",
                    "id": Self::wire_id(id)?,
                    "attachment_id": attachment_id,
                    "data_base64": wire::encode_base64(data),
                }),
                false,
            )
            .await?;
        Self::expect_ok(response)
    }

    /// `resize(id, cols, rows, attachmentID?)` (`persistentPty.update`); selects the terminal.
    pub async fn resize(&self, id: &str, cols: u64, rows: u64, attachment_id: Option<&str>) -> Result<()> {
        let terminal = self.get(id).await?;
        let response = self
            .request(
                json!({
                    "op": "resize",
                    "id": Self::wire_id(id)?,
                    "attachment_id": attachment_id,
                    "cols": cols,
                    "rows": rows,
                }),
                false,
            )
            .await?;
        Self::expect_ok(response)?;
        self.inner.current.lock().insert(terminal.session_id, id.to_string());
        Ok(())
    }

    /// `control(id, attachmentID, cols, rows)` (framed input type 0); selects the terminal.
    pub async fn control(&self, id: &str, attachment_id: &str, cols: u64, rows: u64) -> Result<()> {
        let terminal = self.get(id).await?;
        let response = self
            .request(
                json!({
                    "op": "control",
                    "id": Self::wire_id(id)?,
                    "attachment_id": attachment_id,
                    "cols": cols,
                    "rows": rows,
                }),
                false,
            )
            .await?;
        Self::expect_ok(response)?;
        self.inner.current.lock().insert(terminal.session_id, id.to_string());
        Ok(())
    }

    /// `input(id, attachmentID, cols, rows, data)` (framed input type 1 / legacy input); selects the terminal.
    pub async fn input(&self, id: &str, attachment_id: &str, cols: u64, rows: u64, data: &[u8]) -> Result<()> {
        let terminal = self.get(id).await?;
        let response = self
            .request(
                json!({
                    "op": "input",
                    "id": Self::wire_id(id)?,
                    "attachment_id": attachment_id,
                    "cols": cols,
                    "rows": rows,
                    "data_base64": wire::encode_base64(data),
                }),
                false,
            )
            .await?;
        Self::expect_ok(response)?;
        self.inner.current.lock().insert(terminal.session_id, id.to_string());
        Ok(())
    }

    /// `snapshot(id)`.
    pub async fn snapshot(&self, id: &str) -> Result<Snapshot> {
        self.get(id).await?;
        let response = self.request(json!({ "op": "snapshot", "id": Self::wire_id(id)? }), false).await?;
        let wire::WireResponse::Snapshot { terminal, text, checkpoint_base64, cursor_x, cursor_y } = response else {
            return Err(unexpected(&response));
        };
        Ok(Snapshot {
            info: Info::from_wire(&terminal),
            text,
            checkpoint: wire::decode_base64(&checkpoint_base64),
            cursor: Cursor { x: cursor_x, y: cursor_y },
        })
    }

    /// `read(sessionID, lines?)`: rows of the session's current (controller-selected) terminal.
    pub async fn read(&self, session_id: &str, lines: Option<i64>) -> Result<Option<ReadResult>> {
        if let Some(lines) = lines
            && !(1..=65535).contains(&lines) {
                return Err(Error::Unavailable("lines must be an integer between 1 and 65535".into()));
            }
        let Some(id) = self.inner.current.lock().get(session_id).cloned() else { return Ok(None) };
        let terminal = match self.get(&id).await {
            Ok(terminal) => Some(terminal),
            Err(Error::NotFound(_)) => None,
            Err(error) => return Err(error),
        };
        if terminal.is_none_or(|t| t.session_id != session_id) {
            let mut current = self.inner.current.lock();
            if current.get(session_id) == Some(&id) {
                current.remove(session_id);
            }
            return Ok(None);
        }
        // Let the daemon choose the live height in the same snapshot when lines is omitted.
        let mut request = json!({ "op": "read_rows", "id": Self::wire_id(&id)? });
        if let Some(lines) = lines {
            request["rows"] = lines.into();
        }
        let response = self.request(request, false).await?;
        let wire::WireResponse::Rows { terminal, lines, cursor_x, cursor_y } = response else {
            return Err(unexpected(&response));
        };
        let info = Info::from_wire(&terminal);
        Ok(Some(ReadResult {
            pty_id: info.id,
            title: info.title,
            cwd: info.cwd,
            foreground_process: info.foreground_process,
            screen: Screen {
                text: lines.join("\n"),
                cols: info.size.cols,
                rows: info.size.rows,
                cursor: Cursor { x: cursor_x, y: cursor_y },
            },
        }))
    }

    /// `remove(id)`: `terminate`; publishes `persistent-pty.removed`.
    pub async fn remove(&self, id: &str) -> Result<()> {
        let terminal = self.get(id).await?;
        let response = self.request(json!({ "op": "terminate", "id": Self::wire_id(id)? }), false).await?;
        Self::expect_ok(response)?;
        {
            let mut current = self.inner.current.lock();
            if current.get(&terminal.session_id).map(String::as_str) == Some(id) {
                current.remove(&terminal.session_id);
            }
        }
        self.publish(BusEvent::Removed { session_id: terminal.session_id, pty_id: id.to_string() });
        Ok(())
    }

    /// `shutdown()`: stop the daemon and all terminals (explicit restart/stop/uninstall).
    pub async fn shutdown(&self) -> Result<()> {
        let response = self.inner.daemon.shutdown().await?;
        self.inner.current.lock().clear();
        match response {
            None => Ok(()),
            Some(response) => Self::expect_ok(response),
        }
    }

    /// `handoff()`: prepare a restart handoff; `None` when no daemon is owned.
    pub async fn handoff(&self) -> Result<Option<Handoff>> {
        Ok(self.inner.daemon.handoff().await?)
    }

    /// `attach(id, input)`: subscribe with replay from `cursor`. Call [`Attachment::activate`]
    /// after sending the replay to start live events. A controller attachment selects the
    /// terminal for `read`. An observed `exited` event removes the terminal in the background.
    pub async fn attach(&self, id: &str, input: AttachInput) -> Result<Attachment> {
        self.get(id).await?;
        let attachment = self
            .inner
            .daemon
            .subscribe(
                Self::wire_id(id)?,
                &SubscribeInput {
                    cursor: input.cursor,
                    attachment_id: input.attachment_id,
                    role: input.role,
                    takeover: input.takeover,
                },
            )
            .await?;
        if attachment.role == Role::Controller {
            self.inner.current.lock().insert(attachment.terminal.group_id.clone(), id.to_string());
        }
        let weak = Arc::downgrade(&self.inner);
        let pty_id = id.to_string();
        let hook: daemon::EventHook = Arc::new(move |event: &StreamEvent| {
            if matches!(event, StreamEvent::Exited { .. }) {
                remove_visible_exit(&weak, &pty_id);
            }
        });
        Ok(Attachment {
            info: Info::from_wire(&attachment.terminal),
            role: attachment.role,
            generation: attachment.generation,
            replay: attachment.replay.clone(),
            inner: attachment,
            hook,
        })
    }
}

/// `removeVisibleExit`: background, de-duplicated removal of a terminal seen exiting.
fn remove_visible_exit(weak: &Weak<Inner>, id: &str) {
    let Some(inner) = weak.upgrade() else { return };
    if !inner.removing.lock().insert(id.to_string()) {
        return;
    }
    let id = id.to_string();
    tokio::spawn(async move {
        let service = PersistentPty { inner };
        match service.remove(&id).await {
            Ok(()) | Err(Error::NotFound(_)) => {}
            Err(Error::Unavailable(message)) => {
                tracing::warn!(id = %id, error = %message, "failed to remove visible exited terminal");
            }
        }
        service.inner.removing.lock().remove(&id);
    });
}

/// `binary ??= resolveBinary(global.bin).catch(reset)`.
fn memoized_binary(resolver: BinaryResolver) -> BinaryFn {
    let cache: Arc<tokio::sync::Mutex<Option<String>>> = Arc::new(tokio::sync::Mutex::new(None));
    Arc::new(move || {
        let cache = cache.clone();
        let resolver = resolver.clone();
        Box::pin(async move {
            let mut cached = cache.lock().await;
            if let Some(path) = cached.as_ref() {
                return Ok(path.clone());
            }
            let path = resolver.resolve().await?;
            *cached = Some(path.clone());
            Ok(path)
        })
    })
}

/// `ShellSelect.environment(bin)`: `$SHELL` if it resolves to an existing file (absolute) or is
/// found on `PATH` (+ `bin`); otherwise `/bin/zsh` on macOS, `bash` on `PATH`, else `/bin/sh`.
/// On Windows: first of pwsh, powershell, `$COMSPEC || cmd.exe`.
pub fn shell_environment(bin: &Path) -> String {
    let find = |name: &str| -> Option<String> {
        let mut paths: Vec<PathBuf> =
            std::env::var_os("PATH").map(|p| std::env::split_paths(&p).collect()).unwrap_or_default();
        paths.push(bin.to_path_buf());
        let joined = std::env::join_paths(paths).ok()?;
        let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("/"));
        which::which_in(name, Some(joined), cwd).ok().map(|p| p.to_string_lossy().into_owned())
    };
    if let Some(shell) = std::env::var("SHELL").ok().filter(|s| !s.is_empty()) {
        if Path::new(&shell).is_absolute() {
            if std::fs::metadata(&shell).is_ok_and(|m| m.is_file()) {
                return shell;
            }
        } else if let Some(found) = find(&shell) {
            return found;
        }
    }
    if cfg!(windows) {
        return find("pwsh")
            .or_else(|| find("powershell"))
            .unwrap_or_else(|| std::env::var("COMSPEC").ok().filter(|s| !s.is_empty()).unwrap_or_else(|| "cmd.exe".into()));
    }
    if cfg!(target_os = "macos") {
        return "/bin/zsh".into();
    }
    find("bash").unwrap_or_else(|| "/bin/sh".into())
}
