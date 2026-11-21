//! The engine side of `experimental.mcp.client` (`server/handlers/mcp.ts` `mcp.client`): attaches the MCP server a
//! client hosts over its WebSocket to the location's MCP registry for as long as the socket stays open.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Weak};

use futures::StreamExt;
use oc_mcp::McpManager;
use oc_mcp::transport::socket::{Socket, SocketTransport};
use oc_server::{McpClientAttachment, McpClientControl, McpClientOutbound};
use parking_lot::Mutex;
use tokio::sync::{mpsc, oneshot, watch};

const LOCATION_SHUT_DOWN: &str = "location shut down";

/// The socket side the transport writes to: frames and close requests queue for the socket's single writer.
struct Outbox {
    tx: mpsc::UnboundedSender<McpClientOutbound>,
    /// A failed attach closes the transport before reporting the error; ignore that close so the socket closes with
    /// 1011 instead of a normal 1000.
    attached: Arc<AtomicBool>,
    manager: Weak<McpManager>,
}

impl Socket for Outbox {
    fn send(&self, text: String) {
        let _ = self.tx.send(McpClientOutbound::Text(text));
    }

    fn close(&self, code: u16) {
        if !self.attached.load(Ordering::SeqCst) {
            return;
        }
        // The location's MCP registry closes its attached connections first when the location shuts down; that close
        // is the shutdown's (`holdLocation`), so the client reconnects to the replacement.
        let closing = self.manager.upgrade().is_some_and(|m| m.is_closing());
        let close = if closing {
            McpClientOutbound::Close { code: 1012, reason: LOCATION_SHUT_DOWN.into() }
        } else {
            McpClientOutbound::Close { code, reason: String::new() }
        };
        let _ = self.tx.send(close);
    }
}

struct Control {
    transport: Arc<SocketTransport>,
    open: Mutex<Option<oneshot::Sender<()>>>,
    disconnected: watch::Sender<bool>,
}

impl McpClientControl for Control {
    fn open(&self) {
        if let Some(open) = self.open.lock().take() {
            let _ = open.send(());
        }
    }

    fn deliver(&self, text: &str) {
        self.transport.deliver(text);
    }

    fn ended(&self) {
        self.disconnected.send_replace(true);
        self.open.lock().take();
        self.transport.ended();
    }
}

/// Attaches `name`'s client-hosted server to `manager` once the socket opens. The socket closes with
/// `1012 "location shut down"` when the location `directory` shuts down (reload, `vcs.init`, debug eviction) and with
/// `1011 "attach failed"` when the handshake or first tool list fails.
pub(crate) fn attach(
    manager: Arc<McpManager>,
    name: String,
    directory: String,
    mut events: tokio::sync::broadcast::Receiver<oc_core::bus::Event>,
) -> McpClientAttachment {
    let (tx, rx) = mpsc::unbounded_channel();
    let attached = Arc::new(AtomicBool::new(false));
    let (transport_tx, transport_rx) = mpsc::unbounded_channel();
    let outbox = Outbox { tx: tx.clone(), attached: attached.clone(), manager: Arc::downgrade(&manager) };
    let transport = SocketTransport::new(Arc::new(outbox), transport_tx);
    let (open_tx, open_rx) = oneshot::channel();
    let (disconnected, mut gone) = watch::channel(false);
    let control = Arc::new(Control { transport: transport.clone(), open: Mutex::new(Some(open_tx)), disconnected });

    // `holdLocation`: the socket borrows its location for as long as it stays open; any shutdown closes it with 1012.
    let shutdown = {
        let tx = tx.clone();
        tokio::spawn(async move {
            loop {
                match events.recv().await {
                    Ok(e) if e.kind == "location.shutdown" && e.location.as_ref().is_some_and(|l| l.directory == directory) => {
                        let _ = tx.send(McpClientOutbound::Close { code: 1012, reason: LOCATION_SHUT_DOWN.into() });
                        return;
                    }
                    Ok(_) | Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                    Err(_) => return,
                }
            }
        })
    };

    tokio::spawn(async move {
        if open_rx.await.is_ok() {
            match manager.attach(&name, transport, transport_rx).await {
                Ok(attachment) => {
                    attached.store(true, Ordering::SeqCst);
                    // The attachment lives as long as the socket.
                    let _ = gone.wait_for(|gone| *gone).await;
                    attachment.close().await;
                }
                Err(error) if *gone.borrow() => {
                    tracing::info!(name = %name, error = %error, "client MCP disconnected during initialize");
                }
                Err(error) => {
                    tracing::error!(name = %name, error = %error, "client MCP attach failed");
                    let close = if manager.is_closing() {
                        McpClientOutbound::Close { code: 1012, reason: LOCATION_SHUT_DOWN.into() }
                    } else {
                        McpClientOutbound::Close { code: 1011, reason: "attach failed".into() }
                    };
                    let _ = tx.send(close);
                }
            }
        }
        shutdown.abort();
    });

    let output = futures::stream::unfold(rx, |mut rx| async move { rx.recv().await.map(|item| (item, rx)) }).boxed();
    McpClientAttachment { output, control }
}
