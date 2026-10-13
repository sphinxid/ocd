//! The sidecar process: spawn, JSON-RPC framing over stdio, request/response matching,
//! cancellation, and log forwarding to `tracing`.

use std::collections::HashMap;
use std::path::Path;
use std::process::Stdio;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use async_trait::async_trait;
use parking_lot::Mutex;
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::sync::{mpsc, oneshot, watch};

use crate::protocol::{Message, WireError};

/// Errors from a request to the sidecar.
#[derive(Debug, Clone, PartialEq, thiserror::Error)]
pub enum CallError {
    /// The sidecar answered with an error.
    #[error("{}", .0.message)]
    Remote(Box<WireError>),
    /// The sidecar is not running or exited before answering.
    #[error("Plugin host is not running: {0}")]
    Closed(String),
}

/// Handles requests and notifications sent by the sidecar.
#[async_trait]
pub trait Incoming: Send + Sync + 'static {
    async fn request(&self, method: String, params: Value) -> Result<Value, WireError>;
    /// Called in arrival order, before any later message is processed.
    fn notification(&self, method: &str, params: Value);
}

type Pending = Arc<Mutex<HashMap<u64, oneshot::Sender<Result<Value, WireError>>>>>;

pub struct Sidecar {
    writer: mpsc::UnboundedSender<String>,
    pending: Pending,
    next: AtomicU64,
    alive: Arc<AtomicBool>,
    exited: watch::Receiver<Option<String>>,
    child: Mutex<Option<tokio::process::Child>>,
    pub pid: Option<u32>,
}

impl Sidecar {
    /// Spawns `bun --use-system-ca <script>` and starts the I/O tasks.
    pub fn spawn(bun: &Path, script: &Path, cwd: &Path, incoming: Arc<dyn Incoming>) -> std::io::Result<Arc<Self>> {
        let mut child = tokio::process::Command::new(bun)
            // Upstream's binary is compiled with this flag, so plugin and AI SDK requests trust the system store.
            .arg("--use-system-ca")
            .arg(script)
            .current_dir(cwd)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()?;
        let pid = child.id();
        let mut stdin = child.stdin.take().expect("piped stdin");
        let stdout = child.stdout.take().expect("piped stdout");
        let stderr = child.stderr.take().expect("piped stderr");
        let (writer, mut lines) = mpsc::unbounded_channel::<String>();
        let pending: Pending = Arc::default();
        let alive = Arc::new(AtomicBool::new(true));
        let (exit_tx, exited) = watch::channel(None);

        tokio::spawn(async move {
            while let Some(line) = lines.recv().await {
                if stdin.write_all(line.as_bytes()).await.is_err() || stdin.flush().await.is_err() {
                    break;
                }
            }
            // Dropping stdin tells the sidecar to shut down.
        });

        tokio::spawn(async move {
            let mut reader = BufReader::new(stderr).lines();
            while let Ok(Some(line)) = reader.next_line().await {
                log_line(&line);
            }
        });

        let sidecar = Arc::new(Self { writer, pending: pending.clone(), next: AtomicU64::new(1), alive: alive.clone(), exited, child: Mutex::new(Some(child)), pid });
        let weak = Arc::downgrade(&sidecar);
        tokio::spawn(async move {
            let mut reader = BufReader::new(stdout).lines();
            loop {
                let line = match reader.next_line().await {
                    Ok(Some(line)) => line,
                    _ => break,
                };
                let Some(sidecar) = weak.upgrade() else { break };
                sidecar.receive(&line, &incoming);
            }
            alive.store(false, Ordering::SeqCst);
            for (_, sender) in pending.lock().drain() {
                let _ = sender.send(Err(WireError::new("Plugin host exited")));
            }
            let status = match weak.upgrade().and_then(|s| s.child.lock().take()) {
                Some(mut child) => match child.wait().await {
                    Ok(status) => status.to_string(),
                    Err(error) => error.to_string(),
                },
                None => "closed".to_string(),
            };
            let _ = exit_tx.send(Some(status));
        });
        Ok(sidecar)
    }

    pub fn is_alive(&self) -> bool {
        self.alive.load(Ordering::SeqCst)
    }

    /// Resolves with the exit status once the process is gone.
    pub async fn wait(&self) -> String {
        let mut exited = self.exited.clone();
        loop {
            if let Some(status) = exited.borrow().clone() {
                return status;
            }
            if exited.changed().await.is_err() {
                return "closed".into();
            }
        }
    }

    fn receive(self: &Arc<Self>, line: &str, incoming: &Arc<dyn Incoming>) {
        let message: Message = match serde_json::from_str(line) {
            Ok(message) => message,
            Err(_) => {
                // Not protocol: something wrote to stdout directly.
                tracing::info!(target: "oc_plugin::plugin", "{line}");
                return;
            }
        };
        match (message.method, message.id) {
            (Some(method), Some(id)) => {
                let incoming = incoming.clone();
                let writer = self.writer.clone();
                tokio::spawn(async move {
                    let result = incoming.request(method, message.params.unwrap_or(Value::Null)).await;
                    let response = Message::response(id, result);
                    if let Ok(text) = serde_json::to_string(&response) {
                        let _ = writer.send(text + "\n");
                    }
                });
            }
            (Some(method), None) => incoming.notification(&method, message.params.unwrap_or(Value::Null)),
            (None, Some(id)) => {
                let Some(id) = id.as_u64() else { return };
                let Some(sender) = self.pending.lock().remove(&id) else { return };
                let _ = sender.send(match message.error {
                    Some(error) => Err(error),
                    None => Ok(message.result.unwrap_or(Value::Null)),
                });
            }
            (None, None) => {}
        }
    }

    pub fn notify(&self, method: &str, params: Value) {
        if let Ok(text) = serde_json::to_string(&Message::notification(method, params)) {
            let _ = self.writer.send(text + "\n");
        }
    }

    /// Sends a request; dropping the future cancels it on the sidecar (`$/cancel`).
    pub async fn request(&self, method: &str, params: Value) -> Result<Value, CallError> {
        self.request_with(method, &params).await
    }

    /// [`Sidecar::request`] with borrowed params, serialized straight into the request line (large
    /// values such as catalog transforms are not copied first).
    pub async fn request_with<P: serde::Serialize + ?Sized>(&self, method: &str, params: &P) -> Result<Value, CallError> {
        if !self.is_alive() {
            return Err(CallError::Closed("exited".into()));
        }
        #[derive(serde::Serialize)]
        struct Request<'a, P: ?Sized> {
            jsonrpc: &'static str,
            id: u64,
            method: &'a str,
            params: &'a P,
        }
        let id = self.next.fetch_add(1, Ordering::SeqCst);
        let text = serde_json::to_string(&Request { jsonrpc: "2.0", id, method, params })
            .map_err(|e| CallError::Remote(Box::new(WireError::new(e.to_string()))))?;
        let (tx, rx) = oneshot::channel();
        self.pending.lock().insert(id, tx);
        if self.writer.send(text + "\n").is_err() {
            self.pending.lock().remove(&id);
            return Err(CallError::Closed("stdin closed".into()));
        }
        struct Guard<'a> {
            sidecar: &'a Sidecar,
            id: u64,
            done: bool,
        }
        impl Drop for Guard<'_> {
            fn drop(&mut self) {
                if !self.done && self.sidecar.pending.lock().remove(&self.id).is_some() {
                    self.sidecar.notify("$/cancel", json!({ "id": self.id }));
                }
            }
        }
        let mut guard = Guard { sidecar: self, id, done: false };
        let result = rx.await;
        guard.done = true;
        match result {
            Ok(Ok(value)) => Ok(value),
            Ok(Err(error)) if error.message == "Plugin host exited" && !self.is_alive() => Err(CallError::Closed("exited".into())),
            Ok(Err(error)) => Err(CallError::Remote(Box::new(error))),
            Err(_) => Err(CallError::Closed("exited".into())),
        }
    }

    /// Asks the sidecar to close every location (running plugin cleanups) and exit; kills it
    /// after `grace`.
    pub async fn shutdown(&self, grace: std::time::Duration) {
        if self.is_alive() {
            let _ = tokio::time::timeout(grace, self.request("shutdown", json!({}))).await;
            if tokio::time::timeout(grace, self.wait()).await.is_ok() {
                return;
            }
        }
        self.kill();
    }

    pub fn kill(&self) {
        if let Some(child) = self.child.lock().as_mut() {
            let _ = child.start_kill();
        }
    }
}

fn log_line(line: &str) {
    let Ok(Value::Object(mut record)) = serde_json::from_str::<Value>(line) else {
        if !line.trim().is_empty() {
            tracing::info!(target: "oc_plugin::plugin", "{line}");
        }
        return;
    };
    let Some(level) = record.remove("$log").and_then(|l| l.as_str().map(String::from)) else {
        tracing::info!(target: "oc_plugin::plugin", "{line}");
        return;
    };
    let message = record.remove("message").and_then(|m| m.as_str().map(String::from)).unwrap_or_default();
    let fields = if record.is_empty() { String::new() } else { Value::Object(record).to_string() };
    match level.as_str() {
        "error" => tracing::error!(target: "oc_plugin::host", fields = %fields, "{message}"),
        "warn" => tracing::warn!(target: "oc_plugin::host", fields = %fields, "{message}"),
        "info" => tracing::info!(target: "oc_plugin::host", fields = %fields, "{message}"),
        "debug" => tracing::debug!(target: "oc_plugin::host", fields = %fields, "{message}"),
        _ => tracing::trace!(target: "oc_plugin::host", fields = %fields, "{message}"),
    }
}
