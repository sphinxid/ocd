//! stdio transport (`packages/core/src/mcp/stdio.ts`): spawns the server in its own process
//! group, frames newline-delimited JSON on stdin/stdout, drains stderr into a 1000-char tail and
//! the debug log, and reports unexpected exits with the exit code and stderr tail.

use std::path::PathBuf;
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use indexmap::IndexMap;
use parking_lot::Mutex;
use serde_json::Value;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::process::Command;
use tokio::sync::{OnceCell, mpsc, watch};

use super::{EventSink, SendOptions, Transport, TransportEvent, TransportKind};
use crate::error::McpError;
use crate::jsonrpc::{Frame, LineBuffer, serialize_line};

/// Wait this long for a graceful exit after stdin closes.
pub const CLOSE_GRACE: Duration = Duration::from_secs(2);
/// Escalate SIGTERM to SIGKILL after this long.
pub const FORCE_KILL_AFTER: Duration = Duration::from_secs(2);
pub const OUTGOING_CAPACITY: usize = 64;
/// An unexpected exit waits this long for the exit code and the final stderr output.
pub const EXIT_WAIT: Duration = Duration::from_secs(1);
pub const STDERR_TAIL: usize = 1_000;

#[derive(Debug, Clone)]
pub struct StdioOptions {
    /// Server name; only used to attribute logs.
    pub server: String,
    pub command: String,
    pub args: Vec<String>,
    pub cwd: PathBuf,
    /// Environment declared by the server config; the host environment is inherited.
    pub environment: IndexMap<String, String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Phase {
    Ready,
    Starting,
    Open,
    Closed,
}

#[derive(Debug, Clone, Copy)]
enum Signal {
    Term,
    Kill,
}

struct Handle {
    exit: watch::Receiver<Option<Option<i32>>>,
    kill: mpsc::UnboundedSender<Signal>,
}

struct State {
    phase: Phase,
    outgoing: Option<mpsc::Sender<String>>,
    handle: Option<Arc<Handle>>,
}

pub struct StdioTransport {
    options: StdioOptions,
    events: EventSink,
    state: Mutex<State>,
    stderr: Arc<Mutex<String>>,
    startup: OnceCell<()>,
    closing: OnceCell<()>,
    this: std::sync::Weak<StdioTransport>,
}

impl StdioTransport {
    pub fn new(options: StdioOptions, events: EventSink) -> Arc<Self> {
        Arc::new_cyclic(|this| Self {
            options,
            events,
            state: Mutex::new(State { phase: Phase::Ready, outgoing: None, handle: None }),
            stderr: Arc::new(Mutex::new(String::new())),
            startup: OnceCell::new(),
            closing: OnceCell::new(),
            this: this.clone(),
        })
    }

    /// The last (up to) 1000 characters the server wrote to stderr.
    pub fn stderr_tail(&self) -> String {
        self.stderr.lock().clone()
    }

    fn emit(&self, event: TransportEvent) {
        let _ = self.events.send(event);
    }

    async fn spawn(&self) -> Result<(), McpError> {
        let mut command = Command::new(&self.options.command);
        command
            .args(&self.options.args)
            .current_dir(&self.options.cwd)
            .envs(&self.options.environment)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        #[cfg(unix)]
        command.process_group(0);
        let mut child = command.spawn().map_err(|error| spawn_error(&self.options.command, &error))?;
        let pid = child.id();
        let stdin = child.stdin.take();
        let stdout = child.stdout.take();
        let stderr = child.stderr.take();

        let (exit_tx, exit_rx) = watch::channel(None);
        let (kill_tx, mut kill_rx) = mpsc::unbounded_channel::<Signal>();
        tokio::spawn(async move {
            loop {
                tokio::select! {
                    status = child.wait() => {
                        let code = status.ok().and_then(|s| s.code());
                        let _ = exit_tx.send(Some(code));
                        // Keep serving kill requests so descendants in the group can be reaped.
                        while let Some(signal) = kill_rx.recv().await {
                            send_signal(pid, signal, None);
                        }
                        break;
                    }
                    signal = kill_rx.recv() => {
                        match signal {
                            Some(signal) => send_signal(pid, signal, Some(&mut child)),
                            None => {}
                        }
                    }
                }
            }
        });
        let handle = Arc::new(Handle { exit: exit_rx, kill: kill_tx });

        let (out_tx, mut out_rx) = mpsc::channel::<String>(OUTGOING_CAPACITY);
        if let Some(mut stdin) = stdin {
            tokio::spawn(async move {
                while let Some(frame) = out_rx.recv().await {
                    if stdin.write_all(frame.as_bytes()).await.is_err() {
                        break;
                    }
                    let _ = stdin.flush().await;
                }
                let _ = stdin.shutdown().await;
            });
        }

        let closed = {
            let mut state = self.state.lock();
            if state.phase == Phase::Closed {
                true
            } else {
                state.handle = Some(handle.clone());
                state.phase = Phase::Open;
                state.outgoing = Some(out_tx.clone());
                false
            }
        };
        drop(out_tx);
        if closed {
            stop(&handle).await;
            return Ok(());
        }

        // Drain stderr into the debug log so chatty servers cannot stall on a full pipe.
        let (drained_tx, drained_rx) = watch::channel(false);
        if let Some(mut stderr) = stderr {
            let tail = self.stderr.clone();
            let server = self.options.server.clone();
            tokio::spawn(async move {
                let mut buf = vec![0u8; 8192];
                loop {
                    match stderr.read(&mut buf).await {
                        Ok(0) | Err(_) => break,
                        Ok(n) => {
                            let output = String::from_utf8_lossy(&buf[..n]).into_owned();
                            {
                                let mut tail = tail.lock();
                                tail.push_str(&output);
                                let count = tail.chars().count();
                                if count > STDERR_TAIL {
                                    *tail = tail.chars().skip(count - STDERR_TAIL).collect();
                                }
                            }
                            if !output.trim().is_empty() {
                                tracing::debug!(server = %server, output = %output, "mcp server stderr");
                            }
                        }
                    }
                }
                let _ = drained_tx.send(true);
            });
        } else {
            let _ = drained_tx.send(true);
        }

        let this = self.this.upgrade().expect("transport alive");
        tokio::spawn(async move {
            if let Some(mut stdout) = stdout {
                let mut buffer = LineBuffer::new();
                let mut chunk = vec![0u8; 64 * 1024];
                'read: loop {
                    match stdout.read(&mut chunk).await {
                        Ok(0) => break,
                        Err(error) => {
                            this.emit(TransportEvent::Error(McpError::other(error.to_string())));
                            break;
                        }
                        Ok(n) => {
                            if let Err(error) = buffer.append(&chunk[..n]) {
                                this.emit(TransportEvent::Error(error));
                                break 'read;
                            }
                            while let Some(frame) = buffer.next_frame() {
                                match frame {
                                    Frame::Message(message) => this.emit(TransportEvent::Message(message)),
                                    Frame::Invalid(error) => this.emit(TransportEvent::Error(error)),
                                }
                            }
                        }
                    }
                }
            }
            // stdout ending means the server is gone.
            let unexpected = this.state.lock().phase != Phase::Closed;
            if unexpected {
                let mut drained = drained_rx;
                let mut exit = handle.exit.clone();
                let code = tokio::time::timeout(EXIT_WAIT, async {
                    let _ = drained.wait_for(|d| *d).await;
                    exit.wait_for(|e| e.is_some()).await.ok().and_then(|e| *e)
                })
                .await
                .ok()
                .flatten()
                .flatten();
                let exited = match code {
                    Some(code) => format!("MCP server process exited with code {code}"),
                    None => "MCP server process exited".to_string(),
                };
                let output = this.stderr.lock().trim().to_string();
                let message = if output.is_empty() { exited } else { format!("{exited}: {output}") };
                this.emit(TransportEvent::Error(McpError::other(message)));
                this.close().await;
            }
            this.emit(TransportEvent::Closed);
        });
        Ok(())
    }
}

fn spawn_error(command: &str, error: &std::io::Error) -> McpError {
    let code = match error.kind() {
        std::io::ErrorKind::NotFound => "ENOENT",
        std::io::ErrorKind::PermissionDenied => "EACCES",
        _ => "",
    };
    if code.is_empty() {
        McpError::other(format!("spawn {command} {error}"))
    } else {
        McpError::Network { message: format!("spawn {command} {code}"), code: None }
    }
}

#[cfg(unix)]
fn send_signal(pid: Option<u32>, signal: Signal, _child: Option<&mut tokio::process::Child>) {
    let Some(pid) = pid else { return };
    let sig = match signal {
        Signal::Term => libc::SIGTERM,
        Signal::Kill => libc::SIGKILL,
    };
    // The child leads its own process group: signal the whole group so descendants go too.
    unsafe {
        libc::kill(-(pid as i32), sig);
    }
}

#[cfg(not(unix))]
fn send_signal(_pid: Option<u32>, _signal: Signal, child: Option<&mut tokio::process::Child>) {
    if let Some(child) = child {
        let _ = child.start_kill();
    }
}

/// Close sequence: wait up to 2 s for exit after stdin ends, then SIGTERM, then SIGKILL after 2 s.
async fn stop(handle: &Handle) {
    let mut exit = handle.exit.clone();
    let _ = tokio::time::timeout(CLOSE_GRACE, exit.wait_for(|e| e.is_some())).await;
    let _ = handle.kill.send(Signal::Term);
    let mut exit = handle.exit.clone();
    let terminated = tokio::time::timeout(FORCE_KILL_AFTER, exit.wait_for(|e| e.is_some())).await;
    if terminated.is_err() {
        let _ = handle.kill.send(Signal::Kill);
    }
}

#[async_trait]
impl Transport for StdioTransport {
    async fn start(&self) -> Result<(), McpError> {
        {
            let mut state = self.state.lock();
            if state.phase != Phase::Ready {
                return Err(McpError::other("Stdio transport already started"));
            }
            state.phase = Phase::Starting;
        }
        let mut result = Ok(());
        self.startup
            .get_or_init(|| async {
                result = self.spawn().await;
            })
            .await;
        result
    }

    async fn send(&self, message: Value, _options: SendOptions) -> Result<(), McpError> {
        let sender = {
            let state = self.state.lock();
            if state.phase != Phase::Open {
                return Err(McpError::other("Not connected"));
            }
            state.outgoing.clone()
        };
        let Some(sender) = sender else { return Err(McpError::other("Not connected")) };
        sender.send(serialize_line(&message)).await.map_err(|_| McpError::other("Not connected"))
    }

    async fn close(&self) {
        self.closing
            .get_or_init(|| async {
                {
                    let mut state = self.state.lock();
                    state.phase = Phase::Closed;
                    // Ending the queue closes stdin.
                    state.outgoing = None;
                }
                // A spawn still in flight observes `Closed` and stops the process itself.
                let handle = self.state.lock().handle.take();
                if let Some(handle) = handle {
                    stop(&handle).await;
                }
            })
            .await;
    }

    fn kind(&self) -> TransportKind {
        TransportKind::Stdio
    }
}
