//! A running tunnel (`client/src/effect/tunnel.ts`): one bridge session at a time, reconnecting with backoff until
//! closed or a fatal attach error. Each public connection the relay opens is a TLS session that terminates here,
//! with the tunnel's own certificate, and is forwarded to the route's `host:port` target.

use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use futures::{SinkExt, StreamExt};
use parking_lot::Mutex;
use serde::Serialize;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::{Notify, mpsc, oneshot, watch};
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::protocol::CloseFrame;
use tokio_tungstenite::tungstenite::protocol::frame::coding::CloseCode;

use crate::api::{Api, USER_AGENT};
use crate::bridge::{self, ServerMessage, code, timing};
use crate::names;
use crate::storage::{Identity, parse_date};

/// Bytes queued on the WebSocket before local sockets stop being read.
const HIGH_WATER_MARK: usize = 1024 * 1024;
const LOW_WATER_MARK: usize = 256 * 1024;
/// Bytes buffered for one public connection before it is treated as stalled.
const STALLED_CHANNEL_BYTES: usize = 8 * 1024 * 1024;
const DAY_MS: i64 = 24 * 60 * 60 * 1000;
/// How often to check for a renewed certificate: rarely while the current one is fresh, every minute once renewal
/// is due (it completes within minutes).
const CERTIFICATE_CHECK_FRESH: Duration = Duration::from_secs(12 * 60 * 60);
const CERTIFICATE_CHECK_DUE: Duration = Duration::from_secs(60);
const RENEWAL_WINDOW_MS: i64 = 30 * DAY_MS;

pub type Routes = BTreeMap<String, String>;

/// Tunnel events, as the SDK's `ClientEvent`.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(tag = "type", rename_all = "kebab-case")]
pub enum Event {
    Connecting {
        attempt: u32,
    },
    Connected {
        session: String,
        routes: Vec<String>,
    },
    Disconnected {
        reason: String,
    },
    Reconnecting {
        attempt: u32,
        #[serde(rename = "delayMs")]
        delay_ms: u64,
    },
    ConnectionOpened {
        conn: u32,
        route: String,
        peer: String,
    },
    ConnectionClosed {
        conn: u32,
        route: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        error: Option<String>,
    },
    CertificateRenewed {
        expiry: String,
    },
    Stopped {
        #[serde(skip_serializing_if = "Option::is_none")]
        error: Option<String>,
    },
}

/// `Bridge attach failed: <code>` (`OpenTunnelAttachError`), which stops the tunnel for good.
#[derive(Debug, Clone, PartialEq)]
pub struct AttachError {
    pub code: String,
}

impl std::fmt::Display for AttachError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Bridge attach failed: {}", self.code)
    }
}

/// Codes that stop the tunnel instead of reconnecting.
fn fatal_attach_code(value: &str) -> bool {
    value == code::BAD_TOKEN || value == code::CERT_NOT_READY
}

#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct Status {
    pub state: String,
    pub hostname: String,
    pub routes: Routes,
    pub connections: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session: Option<String>,
    #[serde(rename = "lastError", skip_serializing_if = "Option::is_none")]
    pub last_error: Option<String>,
    #[serde(rename = "connectedAt", skip_serializing_if = "Option::is_none")]
    pub connected_at: Option<i64>,
}

/// The TLS server configuration for an identity (`Tls.createSecureContext({key, cert: certificate + "\n" + chain})`).
pub fn server_config(identity: &Identity) -> Result<Arc<rustls::ServerConfig>, String> {
    use rustls::pki_types::pem::PemObject;
    use rustls::pki_types::{CertificateDer, PrivateKeyDer};
    let bundle = format!("{}\n{}", identity.certificate, identity.chain);
    let certs: Vec<CertificateDer<'static>> = CertificateDer::pem_slice_iter(bundle.as_bytes())
        .collect::<Result<_, _>>()
        .map_err(|e| format!("Invalid tunnel certificate: {e:?}"))?;
    if certs.is_empty() {
        return Err("Invalid tunnel certificate: no certificate".into());
    }
    let key = PrivateKeyDer::from_pem_slice(identity.private_key.as_bytes())
        .map_err(|e| format!("Invalid tunnel private key: {e:?}"))?;
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let config = rustls::ServerConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .map_err(|e| e.to_string())?
        .with_no_client_auth()
        .with_single_cert(certs, key)
        .map_err(|e| format!("Invalid tunnel certificate: {e}"))?;
    Ok(Arc::new(config))
}

fn now_ms() -> i64 {
    chrono::Utc::now().timestamp_millis()
}

fn backoff(attempt: u32) -> u64 {
    let base = (timing::RECONNECT_BACKOFF_MIN_MS as f64 * 2f64.powi(attempt.min(16) as i32))
        .min(timing::RECONNECT_BACKOFF_MAX_MS as f64);
    (base / 2.0 + rand::random::<f64>() * (base / 2.0)).round() as u64
}

fn same_names(left: &Routes, right: &Routes) -> bool {
    left.keys().eq(right.keys())
}

type EventSink = Arc<dyn Fn(Event) + Send + Sync>;
type RenewSink = Arc<dyn Fn(Identity) + Send + Sync>;

pub struct Options {
    pub api: Api,
    pub identity: Identity,
    pub routes: Routes,
    pub on_event: EventSink,
    pub on_renewed: RenewSink,
}

#[derive(Debug, Clone, PartialEq)]
enum End {
    Shutdown,
    RoutesChanged,
    Fatal(AttachError),
    Retry { error: String, attached: bool },
}

struct Shared {
    api: Api,
    on_event: EventSink,
    on_renewed: RenewSink,
    identity: Mutex<Identity>,
    tls: Mutex<Arc<rustls::ServerConfig>>,
    routes: Mutex<Routes>,
    status: Mutex<Status>,
    shutdown: watch::Sender<bool>,
    /// Ends the current session early (routes changed or shutdown).
    interrupt: Mutex<Option<oneshot::Sender<End>>>,
    wake: Notify,
}

impl Shared {
    fn emit(&self, event: Event) {
        (self.on_event)(event)
    }

    fn is_shutdown(&self) -> bool {
        *self.shutdown.borrow()
    }
}

/// How the tunnel ended.
pub type Closed = Result<(), AttachError>;

/// A running tunnel.
pub struct Tunnel {
    shared: Arc<Shared>,
    ready: Option<oneshot::Receiver<Closed>>,
    closed: watch::Receiver<Option<Closed>>,
}

impl Tunnel {
    /// Starts the tunnel. Fails only when the identity's certificate or key cannot be loaded.
    pub fn start(options: Options) -> Result<Tunnel, String> {
        names::validate_routes(options.routes.iter())?;
        let tls = server_config(&options.identity)?;
        let (shutdown, _) = watch::channel(false);
        let shared = Arc::new(Shared {
            api: options.api,
            on_event: options.on_event,
            on_renewed: options.on_renewed,
            status: Mutex::new(Status {
                state: "connecting".into(),
                hostname: options.identity.hostname.clone(),
                routes: options.routes.clone(),
                ..Default::default()
            }),
            identity: Mutex::new(options.identity),
            tls: Mutex::new(tls),
            routes: Mutex::new(options.routes),
            shutdown,
            interrupt: Mutex::new(None),
            wake: Notify::new(),
        });
        let (ready_tx, ready_rx) = oneshot::channel();
        let (closed_tx, closed_rx) = watch::channel(None);
        let run_shared = shared.clone();
        tokio::spawn(async move {
            let ready = Arc::new(Mutex::new(Some(ready_tx)));
            let result = run(run_shared, ready.clone()).await;
            if let Some(tx) = ready.lock().take() {
                let _ = tx.send(result.clone());
            }
            let _ = closed_tx.send(Some(result));
        });
        tokio::spawn(refresh_certificate(shared.clone()));
        Ok(Tunnel { shared, ready: Some(ready_rx), closed: closed_rx })
    }

    /// Resolves once the bridge first attaches (or the tunnel is closed first); fails on a fatal attach error.
    pub async fn ready(&mut self) -> Closed {
        match self.ready.take() {
            Some(rx) => rx.await.unwrap_or(Ok(())),
            None => Ok(()),
        }
    }

    /// Resolves when the tunnel stops: `Ok` after [`Tunnel::close`], the attach error otherwise.
    pub fn closed(&self) -> impl std::future::Future<Output = Closed> + Send + 'static {
        let mut rx = self.closed.clone();
        async move {
            loop {
                if let Some(result) = rx.borrow().clone() {
                    return result;
                }
                if rx.changed().await.is_err() {
                    return Ok(());
                }
            }
        }
    }

    pub fn status(&self) -> Status {
        self.shared.status.lock().clone()
    }

    pub fn identity(&self) -> Identity {
        self.shared.identity.lock().clone()
    }

    /// Replaces the routes. Changing only targets applies to new connections immediately; adding or removing names
    /// re-attaches the bridge.
    pub fn set_routes(&self, routes: Routes) -> Result<(), String> {
        names::validate_routes(routes.iter())?;
        let changed = {
            let mut current = self.shared.routes.lock();
            let changed = !same_names(&current, &routes);
            *current = routes.clone();
            changed
        };
        self.shared.status.lock().routes = routes;
        if changed && let Some(tx) = self.shared.interrupt.lock().take() {
            let _ = tx.send(End::RoutesChanged);
        }
        self.shared.wake.notify_waiters();
        Ok(())
    }

    /// Stops the tunnel and waits for it to finish.
    pub async fn close(&self) {
        let _ = self.shared.shutdown.send(true);
        if let Some(tx) = self.shared.interrupt.lock().take() {
            let _ = tx.send(End::Shutdown);
        }
        self.shared.wake.notify_waiters();
        let _ = self.closed().await;
    }
}

impl Drop for Tunnel {
    fn drop(&mut self) {
        let _ = self.shared.shutdown.send(true);
        if let Some(tx) = self.shared.interrupt.lock().take() {
            let _ = tx.send(End::Shutdown);
        }
        self.shared.wake.notify_waiters();
    }
}

async fn sleep_or_wake(shared: &Shared, delay: Duration) {
    let mut shutdown = shared.shutdown.subscribe();
    tokio::select! {
        _ = tokio::time::sleep(delay) => {}
        _ = shared.wake.notified() => {}
        _ = shutdown.wait_for(|v| *v) => {}
    }
}

async fn run(shared: Arc<Shared>, ready: Arc<Mutex<Option<oneshot::Sender<Closed>>>>) -> Closed {
    let mut attempt: u32 = 0;
    while !shared.is_shutdown() {
        if shared.routes.lock().is_empty() {
            shared.status.lock().state = "waiting-routes".into();
            sleep_or_wake(&shared, Duration::from_secs(u32::MAX as u64 / 2)).await;
            continue;
        }
        shared.status.lock().state = "connecting".into();
        shared.emit(Event::Connecting { attempt });
        let end = session(&shared, &ready).await;
        {
            let mut status = shared.status.lock();
            status.session = None;
            status.connections = 0;
        }
        match end {
            End::Shutdown => break,
            End::Fatal(error) => {
                {
                    let mut status = shared.status.lock();
                    status.state = "stopped".into();
                    status.last_error = Some(error.to_string());
                }
                shared.emit(Event::Stopped { error: Some(error.to_string()) });
                return Err(error);
            }
            End::RoutesChanged => {
                shared.emit(Event::Disconnected { reason: "routes changed".into() });
                attempt = 0;
                continue;
            }
            End::Retry { error, attached } => {
                if attached {
                    attempt = 0;
                }
                shared.emit(Event::Disconnected { reason: error.clone() });
                let delay = backoff(attempt);
                attempt += 1;
                {
                    let mut status = shared.status.lock();
                    status.state = "reconnecting".into();
                    status.last_error = Some(error);
                }
                shared.emit(Event::Reconnecting { attempt, delay_ms: delay });
                sleep_or_wake(&shared, Duration::from_millis(delay)).await;
            }
        }
    }
    shared.status.lock().state = "stopped".into();
    shared.emit(Event::Stopped { error: None });
    Ok(())
}

/// Fetches the server's current certificate and, when the server has renewed it, uses it for new connections.
/// Checks on start, then on a timer.
async fn refresh_certificate(shared: Arc<Shared>) {
    loop {
        if shared.is_shutdown() {
            return;
        }
        let (id, token, current) = {
            let identity = shared.identity.lock();
            (identity.id.clone(), identity.token.clone(), identity.certificate.clone())
        };
        if let Some(state) = shared.api.current_certificate(&token, &id).await {
            let text = |key: &str| state.get(key).and_then(|v| v.as_str()).filter(|s| !s.is_empty());
            if state.get("type").and_then(|v| v.as_str()) == Some("ready")
                && let (Some(certificate), Some(expiry)) = (text("certificate"), text("expiry"))
                && certificate != current
                && let Some(expiry_date) = parse_date(expiry)
            {
                let renewed = Identity {
                    certificate: certificate.to_string(),
                    chain: state.get("chain").and_then(|v| v.as_str()).unwrap_or("").to_string(),
                    certificate_expiry: expiry_date,
                    ..shared.identity.lock().clone()
                };
                // A certificate that does not load keeps the current one serving.
                if let Ok(config) = server_config(&renewed) {
                    *shared.tls.lock() = config;
                    *shared.identity.lock() = renewed.clone();
                    (shared.on_renewed)(renewed);
                    shared.emit(Event::CertificateRenewed { expiry: expiry.to_string() });
                }
            }
        }
        if shared.is_shutdown() {
            return;
        }
        let expiry = shared.identity.lock().certificate_expiry.timestamp_millis();
        let due = expiry - now_ms() < RENEWAL_WINDOW_MS;
        let delay = if due { CERTIFICATE_CHECK_DUE } else { CERTIFICATE_CHECK_FRESH };
        let mut shutdown = shared.shutdown.subscribe();
        tokio::select! {
            _ = tokio::time::sleep(delay) => {}
            _ = shutdown.wait_for(|v| *v) => return,
        }
    }
}

/// The bridge WebSocket's send queue, with the byte count Bun reports as `bufferedAmount`.
#[derive(Clone)]
struct Outbound {
    tx: mpsc::UnboundedSender<Message>,
    buffered: Arc<AtomicUsize>,
    drained: Arc<Notify>,
    open: watch::Receiver<bool>,
}

impl Outbound {
    fn is_open(&self) -> bool {
        *self.open.borrow()
    }

    fn control(&self, text: String) {
        if self.is_open() {
            let _ = self.tx.send(Message::Text(text.into()));
        }
    }

    /// Sends data for a connection and waits while the WebSocket holds more than the high-water mark.
    async fn data(&self, conn: u32, data: &[u8]) -> Result<(), ()> {
        if !self.is_open() {
            return Err(());
        }
        for chunk in data.chunks(bridge::MAX_PAYLOAD_SIZE) {
            let frame = bridge::build_data_frame(conn, chunk);
            self.buffered.fetch_add(frame.len(), Ordering::SeqCst);
            if self.tx.send(Message::Binary(frame.into())).is_err() {
                return Err(());
            }
        }
        if self.buffered.load(Ordering::SeqCst) < HIGH_WATER_MARK {
            return Ok(());
        }
        let mut open = self.open.clone();
        loop {
            let notified = self.drained.notified();
            if self.buffered.load(Ordering::SeqCst) <= LOW_WATER_MARK || !self.is_open() {
                return Ok(());
            }
            tokio::select! {
                _ = notified => {}
                _ = open.wait_for(|v| !*v) => return Ok(()),
            }
        }
    }
}

enum Inbound {
    Data(bytes::Bytes),
    End,
}

struct Channel {
    route: String,
    inbound: mpsc::UnboundedSender<Inbound>,
    queued: Arc<AtomicUsize>,
    task: tokio::task::AbortHandle,
}

/// A channel's report to its session once it has fully closed.
struct ChannelClosed {
    conn: u32,
    error: Option<String>,
    /// The reset to send first (`destroy(code, notify = true)`).
    reset: Option<&'static str>,
}

async fn session(shared: &Arc<Shared>, ready: &Arc<Mutex<Option<oneshot::Sender<Closed>>>>) -> End {
    let (interrupt_tx, mut interrupt_rx) = oneshot::channel();
    *shared.interrupt.lock() = Some(interrupt_tx);
    if shared.is_shutdown() {
        shared.interrupt.lock().take();
        return End::Shutdown;
    }
    let attach_deadline = tokio::time::Instant::now() + Duration::from_millis(timing::CONNECT_TIMEOUT_MS);
    let (id, token, hostname) = {
        let identity = shared.identity.lock();
        (identity.id.clone(), identity.token.clone(), identity.hostname.clone())
    };
    let url = shared.api.connect_url(&id);
    let connecting = crate::ws::connect(&url, bridge::WEBSOCKET_SUBPROTOCOL, &[("User-Agent", USER_AGENT)]);
    let socket = tokio::select! {
        result = connecting => match result {
            Ok(socket) => socket,
            Err(failed) => {
                shared.interrupt.lock().take();
                return End::Retry { error: format!("bridge closed: {} {}", failed.code, failed.reason).trim().to_string(), attached: false };
            }
        },
        _ = tokio::time::sleep_until(attach_deadline) => {
            shared.interrupt.lock().take();
            return End::Retry { error: "bridge attach timed out".into(), attached: false };
        }
        end = &mut interrupt_rx => return end.unwrap_or(End::Shutdown),
    };
    let (mut sink, mut stream) = socket.split();

    let (open_tx, open_rx) = watch::channel(true);
    let (out_tx, mut out_rx) = mpsc::unbounded_channel::<Message>();
    let outbound = Outbound {
        tx: out_tx,
        buffered: Arc::new(AtomicUsize::new(0)),
        drained: Arc::new(Notify::new()),
        open: open_rx,
    };
    // The writer: sends queued frames in order and reports drained bytes.
    let writer_buffered = outbound.buffered.clone();
    let writer_drained = outbound.drained.clone();
    let (close_tx, close_rx) = oneshot::channel::<()>();
    let writer = tokio::spawn(async move {
        let mut close_rx = close_rx;
        loop {
            tokio::select! {
                biased;
                message = out_rx.recv() => {
                    let Some(message) = message else { break };
                    let size = if let Message::Binary(b) = &message { b.len() } else { 0 };
                    let sent = sink.send(message).await;
                    if size > 0 {
                        writer_buffered.fetch_sub(size, Ordering::SeqCst);
                        writer_drained.notify_waiters();
                    }
                    if sent.is_err() {
                        break;
                    }
                }
                _ = &mut close_rx => {
                    let _ = sink
                        .send(Message::Close(Some(CloseFrame { code: CloseCode::Normal, reason: "".into() })))
                        .await;
                    let _ = sink.flush().await;
                    break;
                }
            }
        }
        sink
    });

    let mut routes: Vec<String> = shared.routes.lock().keys().cloned().collect();
    routes.sort();
    outbound.control(bridge::attach(&token, &routes));

    let mut attached = false;
    let mut last_received = Instant::now();
    let mut heartbeat: Option<(tokio::time::Interval, Duration)> = None;
    let mut channels: HashMap<u32, Channel> = HashMap::new();
    let (closed_tx, mut closed_rx) = mpsc::unbounded_channel::<ChannelClosed>();

    let set_connections = |count: usize| shared.status.lock().connections = count;

    let end = loop {
        let tick = async {
            match heartbeat.as_mut() {
                Some((interval, _)) => {
                    interval.tick().await;
                }
                None => std::future::pending().await,
            }
        };
        let deadline = async {
            if attached { std::future::pending::<()>().await } else { tokio::time::sleep_until(attach_deadline).await }
        };
        tokio::select! {
            end = &mut interrupt_rx => break end.unwrap_or(End::Shutdown),
            _ = deadline => break End::Retry { error: "bridge attach timed out".into(), attached: false },
            _ = tick => {
                let idle = heartbeat.as_ref().map(|(_, idle)| *idle).unwrap_or_default();
                if last_received.elapsed() > idle {
                    break End::Retry { error: "bridge idle timeout".into(), attached: true };
                }
                outbound.control(bridge::ping(now_ms()));
            }
            closed = closed_rx.recv() => {
                let Some(closed) = closed else { continue };
                if let Some(code) = closed.reset {
                    outbound.control(bridge::reset(closed.conn, code));
                }
                if let Some(channel) = channels.remove(&closed.conn) {
                    set_connections(channels.len());
                    shared.emit(Event::ConnectionClosed { conn: closed.conn, route: channel.route, error: closed.error });
                }
            }
            message = stream.next() => {
                let message = match message {
                    Some(Ok(m)) => m,
                    Some(Err(_)) | None => break End::Retry { error: "bridge closed: 1006".into(), attached },
                };
                match message {
                    Message::Text(text) => {
                        last_received = Instant::now();
                        let Some(message) = bridge::decode_server_message(text.as_str()) else { continue };
                        if !attached {
                            match message {
                                ServerMessage::Attached { session, routes, heartbeat_ms, idle_timeout_ms } => {
                                    attached = true;
                                    let period = Duration::from_millis(heartbeat_ms.max(1000.0) as u64);
                                    let idle = Duration::from_millis(idle_timeout_ms.max(5000.0) as u64);
                                    let mut interval = tokio::time::interval_at(tokio::time::Instant::now() + period, period);
                                    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
                                    heartbeat = Some((interval, idle));
                                    {
                                        let mut status = shared.status.lock();
                                        status.state = "connected".into();
                                        status.session = Some(session.clone());
                                        status.last_error = None;
                                        status.connected_at = Some(now_ms());
                                    }
                                    if let Some(tx) = ready.lock().take() {
                                        let _ = tx.send(Ok(()));
                                    }
                                    shared.emit(Event::Connected { session, routes });
                                }
                                ServerMessage::AttachError { code } => {
                                    break if fatal_attach_code(&code) {
                                        End::Fatal(AttachError { code })
                                    } else {
                                        End::Retry { error: format!("attach failed: {code}"), attached: false }
                                    };
                                }
                                _ => {}
                            }
                            continue;
                        }
                        match message {
                            ServerMessage::Open { conn, sni, peer, .. } => {
                                let route = names::route_for_sni(&sni, &hostname);
                                let target = route.as_ref().and_then(|r| shared.routes.lock().get(r).cloned());
                                let parsed = target.as_deref().and_then(names::parse_target);
                                let (Some(route), Some(target)) = (route, parsed) else {
                                    outbound.control(bridge::reset(conn, code::UNKNOWN_ROUTE));
                                    continue;
                                };
                                if channels.len() >= bridge::MAX_CONNS {
                                    outbound.control(bridge::reset(conn, code::TOO_MANY_CONNECTIONS));
                                    continue;
                                }
                                let (inbound_tx, inbound_rx) = mpsc::unbounded_channel();
                                let queued = Arc::new(AtomicUsize::new(0));
                                let tls = shared.tls.lock().clone();
                                let task = tokio::spawn(channel(conn, target, tls, inbound_rx, queued.clone(), outbound.clone(), closed_tx.clone()));
                                if let Some(old) = channels.insert(conn, Channel { route: route.clone(), inbound: inbound_tx, queued, task: task.abort_handle() }) {
                                    old.task.abort();
                                }
                                set_connections(channels.len());
                                shared.emit(Event::ConnectionOpened { conn, route, peer });
                            }
                            ServerMessage::End { conn } => {
                                if let Some(channel) = channels.get(&conn) {
                                    let _ = channel.inbound.send(Inbound::End);
                                }
                            }
                            ServerMessage::Reset { conn, code } => {
                                if let Some(channel) = channels.remove(&conn) {
                                    channel.task.abort();
                                    set_connections(channels.len());
                                    shared.emit(Event::ConnectionClosed { conn, route: channel.route, error: Some(code) });
                                }
                            }
                            ServerMessage::Ping { time_sent } => outbound.control(bridge::pong(&time_sent)),
                            ServerMessage::Drain { reason } => {
                                break End::Retry { error: format!("server draining: {reason}"), attached: true };
                            }
                            _ => {}
                        }
                    }
                    Message::Binary(data) => {
                        last_received = Instant::now();
                        let Some((conn, payload)) = bridge::parse_data_frame(&data) else { continue };
                        let Some(channel) = channels.get(&conn) else { continue };
                        if channel.queued.load(Ordering::SeqCst) > STALLED_CHANNEL_BYTES {
                            let channel = channels.remove(&conn).expect("channel");
                            channel.task.abort();
                            outbound.control(bridge::reset(conn, code::UPSTREAM_IO_ERROR));
                            set_connections(channels.len());
                            shared.emit(Event::ConnectionClosed { conn, route: channel.route, error: Some("local connection stalled".into()) });
                            continue;
                        }
                        channel.queued.fetch_add(payload.len(), Ordering::SeqCst);
                        let _ = channel.inbound.send(Inbound::Data(bytes::Bytes::copy_from_slice(payload)));
                    }
                    Message::Close(frame) => {
                        let (code, reason) = match frame {
                            Some(f) => (u16::from(f.code), f.reason.to_string()),
                            None => (1005, String::new()),
                        };
                        break End::Retry { error: format!("bridge closed: {code} {reason}").trim().to_string(), attached };
                    }
                    _ => {}
                }
            }
        }
    };
    shared.interrupt.lock().take();
    // Tear down: channels see the bridge disconnect, then the socket closes with 1000.
    let _ = open_tx.send(false);
    for (conn, channel) in channels.drain() {
        channel.task.abort();
        shared.emit(Event::ConnectionClosed { conn, route: channel.route, error: Some("bridge disconnected".into()) });
    }
    let _ = close_tx.send(());
    if let Ok(Ok(mut sink)) = tokio::time::timeout(Duration::from_secs(2), writer).await {
        let _ = sink.close().await;
    }
    end
}

/// Terminates TLS for one public connection and forwards it to the target.
async fn channel(
    conn: u32,
    target: names::Target,
    tls: Arc<rustls::ServerConfig>,
    mut inbound: mpsc::UnboundedReceiver<Inbound>,
    queued: Arc<AtomicUsize>,
    outbound: Outbound,
    closed: mpsc::UnboundedSender<ChannelClosed>,
) {
    let (tls_side, bridge_side) = tokio::io::duplex(64 * 1024);
    let (mut bridge_read, mut bridge_write) = tokio::io::split(bridge_side);
    // Relay bytes (and the relay's half-close) into the TLS session.
    let pump_in = tokio::spawn(async move {
        while let Some(item) = inbound.recv().await {
            match item {
                Inbound::Data(bytes) => {
                    let n = bytes.len();
                    let ok = bridge_write.write_all(&bytes).await.is_ok();
                    queued.fetch_sub(n, Ordering::SeqCst);
                    if !ok {
                        break;
                    }
                }
                Inbound::End => {
                    let _ = bridge_write.shutdown().await;
                    break;
                }
            }
        }
        // Keep the write half open until the session ends the channel (a dropped half would read as EOF).
        std::future::pending::<()>().await;
    });
    // TLS records to the relay; the TLS side's end becomes `end` (unless the channel failed, which resets instead).
    let failed = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let out = outbound.clone();
    let pump_failed = failed.clone();
    let pump_out = tokio::spawn(async move {
        let mut buffer = vec![0u8; bridge::MAX_PAYLOAD_SIZE];
        loop {
            match bridge_read.read(&mut buffer).await {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    if out.data(conn, &buffer[..n]).await.is_err() {
                        return;
                    }
                }
            }
        }
        if !pump_failed.load(Ordering::SeqCst) {
            out.control(bridge::end(conn));
        }
    });
    let result = forward(tls_side, target, tls, &failed).await;
    let report = match result {
        Ok(()) => {
            let _ = pump_out.await;
            ChannelClosed { conn, error: None, reset: None }
        }
        Err((code, error)) => {
            pump_out.abort();
            ChannelClosed { conn, error: Some(error), reset: Some(code) }
        }
    };
    pump_in.abort();
    let _ = closed.send(report);
}

/// Runs one connection. On failure `failed` is set before the TLS side is dropped, so its end is not reported.
async fn forward(
    io: tokio::io::DuplexStream,
    target: names::Target,
    tls: Arc<rustls::ServerConfig>,
    failed: &std::sync::atomic::AtomicBool,
) -> Result<(), (&'static str, String)> {
    let fail = |code: &'static str, error: String| {
        failed.store(true, Ordering::SeqCst);
        (code, error)
    };
    let acceptor = tokio_rustls::TlsAcceptor::from(tls);
    let secure = match acceptor.accept(io).into_fallible().await {
        Ok(secure) => secure,
        Err((e, io)) => {
            let error = fail(code::UPSTREAM_IO_ERROR, e.to_string());
            drop(io);
            return Err(error);
        }
    };
    let host = target.host.clone();
    let upstream = match tokio::net::TcpStream::connect((host.as_str(), target.port)).await {
        Ok(upstream) => upstream,
        Err(e) => return Err(fail(code::UPSTREAM_CONNECT_FAILED, e.to_string())),
    };
    let _ = upstream.set_nodelay(true);
    let (mut tls_read, mut tls_write) = tokio::io::split(secure);
    let (mut up_read, mut up_write) = upstream.into_split();
    // Public to local. Like Node, a client that closes without close_notify has simply ended.
    let inbound = async {
        let mut buffer = vec![0u8; 16 * 1024];
        loop {
            let n = match tls_read.read(&mut buffer).await {
                Ok(n) => n,
                Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => 0,
                Err(e) => return Err(e),
            };
            if n == 0 {
                let _ = up_write.shutdown().await;
                return Ok(());
            }
            up_write.write_all(&buffer[..n]).await?;
        }
    };
    // Local to public; the local end closes the TLS session.
    let outbound = async {
        let mut buffer = vec![0u8; 16 * 1024];
        loop {
            let n = up_read.read(&mut buffer).await?;
            if n == 0 {
                let _ = tls_write.shutdown().await;
                return Ok::<(), std::io::Error>(());
            }
            tls_write.write_all(&buffer[..n]).await?;
            tls_write.flush().await?;
        }
    };
    match tokio::try_join!(inbound, outbound) {
        Ok(_) => Ok(()),
        Err(e) => Err(fail(code::UPSTREAM_IO_ERROR, e.to_string())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backoff_stays_in_range() {
        for attempt in 0..40 {
            let base = (250f64 * 2f64.powi(attempt.min(16) as i32)).min(30_000.0);
            let delay = backoff(attempt) as f64;
            assert!(delay >= (base / 2.0).floor() && delay <= base, "{attempt}: {delay}");
        }
    }

    #[test]
    fn events_serialize_like_the_sdk() {
        let json = |e: Event| serde_json::to_string(&e).unwrap();
        assert_eq!(
            json(Event::Reconnecting { attempt: 1, delay_ms: 200 }),
            r#"{"type":"reconnecting","attempt":1,"delayMs":200}"#
        );
        assert_eq!(json(Event::Stopped { error: None }), r#"{"type":"stopped"}"#);
        assert_eq!(
            json(Event::ConnectionClosed { conn: 1, route: "r".into(), error: None }),
            r#"{"type":"connection-closed","conn":1,"route":"r"}"#
        );
    }
}
