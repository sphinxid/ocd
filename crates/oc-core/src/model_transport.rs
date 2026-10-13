//! `SessionModelTransport` (`core/src/session/model-transport.ts`): one Responses WebSocket per
//! session, reused across steps.
//!
//! [`SessionModelTransport::bind`] gives the runner a channel executor for one session. Exchanges of
//! a session run one at a time over the session's socket; the socket is opened on first use, reused
//! while the handshake (URL + headers) stays the same and it is younger than the route's rotation age,
//! and closed after any error frame, on a dropped exchange, when the session is removed, moved or its
//! location closes. The continuation checkpoint of the last completed exchange stays on the socket, so
//! the next step can send only the new input. A failed connect, a 1009 rejection or five consecutive
//! exchanges lost to the socket pin the session to HTTP until restart.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicU8, AtomicU64, Ordering};
use std::time::{Duration, Instant};

use futures::StreamExt;
use oc_ai::AIError;
use oc_ai::error::{ErrorKind, TransportOperation};
use oc_ai::route::Headers;
use oc_ai::route::websocket::{
    ChannelCheckpoint, ChannelExchange, ChannelExecution, ChannelExecutor, ChannelMode, ChannelObservation, DirectConnector,
    FrameStream, WebSocketConnection, WebSocketConnector, WsError, WsMessage, annotate, transport_error,
};
use parking_lot::Mutex;
use sha2::Digest;
use tokio::sync::mpsc;

const ROTATE_AFTER_MS: u64 = 55 * 60 * 1000;
const CONNECT_TIMEOUT: Duration = Duration::from_secs(15);
/// Reasoning models can stream nothing for several minutes while still working.
const IDLE_TIMEOUT: Duration = Duration::from_secs(30 * 60);
/// Consecutive exchanges lost to the socket before the session stays on HTTP.
const MAX_STREAM_FAILURES: u32 = 5;

/// Per-exchange taps (`Interceptor`): `handshake` runs before the connection is selected (its output
/// feeds the affinity key, so changed headers reopen the socket); `send` sees each outbound frame after
/// the driver builds it; `receive` sees each inbound frame before the driver observes it.
#[async_trait::async_trait]
pub trait Interceptor: Send + Sync {
    async fn handshake(&self, url: String, headers: Headers) -> Result<(String, Headers), String> {
        Ok((url, headers))
    }
    async fn send(&self, frame: String) -> Result<String, String> {
        Ok(frame)
    }
    async fn receive(&self, frame: String) -> Result<String, String> {
        Ok(frame)
    }
}

/// How long an exchange may wait for the next frame (`idleTimeout`: the model's `chunkTimeout`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum IdleTimeout {
    /// 30 minutes.
    #[default]
    Default,
    Disabled,
    After(Duration),
}

impl IdleTimeout {
    /// From the resolved model's `chunkTimeout` setting.
    pub fn from_chunk_timeout(value: Option<oc_ai::types::HttpTimeout>) -> Self {
        match value {
            None => Self::Default,
            Some(oc_ai::types::HttpTimeout::Disabled(_)) => Self::Disabled,
            Some(oc_ai::types::HttpTimeout::Millis(ms)) => Self::After(Duration::from_millis(ms.max(0.0) as u64)),
        }
    }

    fn duration(self) -> Option<Duration> {
        match self {
            Self::Default => Some(IDLE_TIMEOUT),
            Self::Disabled => None,
            Self::After(d) => Some(d),
        }
    }
}

const SEND_ATTEMPTED: u8 = 0;
const PROVIDER_OBSERVED: u8 = 1;
const TERMINAL: u8 = 2;

struct Active {
    id: u64,
    tx: mpsc::UnboundedSender<Result<String, AIError>>,
    delivery: Arc<AtomicU8>,
}

#[derive(Default)]
struct ChannelState {
    active: Option<Active>,
    closing: bool,
    checkpoint: Option<ChannelCheckpoint>,
    pending: Option<(u64, ChannelCheckpoint)>,
}

struct Channel {
    affinity: String,
    connection: WebSocketConnection,
    opened_at: Instant,
    state: Mutex<ChannelState>,
    reader: Mutex<Option<tokio::task::AbortHandle>>,
}

#[derive(Default)]
struct OwnerState {
    closed: bool,
    http_fallback: bool,
    stream_failures: u32,
    channel: Option<Arc<Channel>>,
}

struct Owner {
    lock: Arc<tokio::sync::Mutex<()>>,
    state: Mutex<OwnerState>,
}

/// The process-wide session transport (`SessionModelTransport.Service`).
pub struct SessionModelTransport {
    connector: Arc<dyn WebSocketConnector>,
    states: Mutex<HashMap<String, Arc<Owner>>>,
    ids: AtomicU64,
}

impl Default for SessionModelTransport {
    fn default() -> Self {
        Self::new(Arc::new(DirectConnector))
    }
}

fn same(a: &Option<Arc<Channel>>, b: &Arc<Channel>) -> bool {
    a.as_ref().is_some_and(|a| Arc::ptr_eq(a, b))
}

fn accepted(delivery: u8) -> bool {
    delivery == PROVIDER_OBSERVED || delivery == TERMINAL
}

fn error_code(error: &AIError) -> Option<&str> {
    match &error.kind {
        ErrorKind::Transport { code, .. } => code.as_deref(),
        _ => None,
    }
}

fn error_phase(error: &AIError) -> Option<&str> {
    match &error.kind {
        ErrorKind::Transport { phase, .. } => phase.as_deref(),
        _ => None,
    }
}

fn error_delivery(error: &AIError) -> Option<&str> {
    match &error.kind {
        ErrorKind::Transport { delivery, .. } => delivery.as_deref(),
        _ => None,
    }
}

/// `affinity`: the socket URL and a digest of the sorted handshake headers.
fn affinity(url: &str, headers: &Headers) -> String {
    let mut entries: Vec<(&String, &String)> = headers.iter().collect();
    entries.sort_by(|a, b| oc_schema::collate::locale_compare(a.0, b.0));
    let json = serde_json::to_string(&entries.iter().map(|(k, v)| [k.as_str(), v.as_str()]).collect::<Vec<_>>()).unwrap_or_default();
    format!("{url}:{}", hex::encode(sha2::Sha256::digest(json.as_bytes())))
}

fn close_channel(owner: &Owner, channel: &Arc<Channel>) {
    if release_channel(owner, channel) {
        channel.connection.close();
    }
}

/// `closeChannel` without closing the socket: detach the channel and fail its active exchange.
/// False when the channel was already closing.
fn release_channel(owner: &Owner, channel: &Arc<Channel>) -> bool {
    {
        let mut o = owner.state.lock();
        if same(&o.channel, channel) {
            o.channel = None;
        }
    }
    let active = {
        let mut c = channel.state.lock();
        if c.closing {
            return false;
        }
        c.closing = true;
        c.active.as_ref().map(|a| (a.tx.clone(), a.delivery.load(Ordering::SeqCst)))
    };
    if let Some(reader) = channel.reader.lock().take() {
        reader.abort();
    }
    if let Some((tx, delivery)) = active {
        let _ = tx.send(Err(transport_error(
            "Session WebSocket closed",
            TransportOperation::Read,
            WsError { code: Some("close"), phase: Some("close"), delivery: Some(if accepted(delivery) { "accepted" } else { "ambiguous" }), ..Default::default() },
        )));
    }
    tracing::debug!(sessionTransport = "websocket", "session websocket closed");
    true
}

/// A socket that keeps dying mid-exchange costs a retry every step; after enough consecutive losses the
/// session stays on HTTP.
fn stream_failure(owner: &Owner) {
    let mut o = owner.state.lock();
    o.stream_failures += 1;
    if o.stream_failures < MAX_STREAM_FAILURES {
        return;
    }
    o.http_fallback = true;
    tracing::warn!(sessionTransport = "websocket", failures = o.stream_failures, "session websocket failed repeatedly; using http");
}

fn poison(owner: &Owner, channel: &Arc<Channel>, error: AIError) {
    {
        let mut o = owner.state.lock();
        if same(&o.channel, channel) {
            o.channel = None;
        }
    }
    let active = {
        let mut c = channel.state.lock();
        if c.closing {
            return;
        }
        c.closing = true;
        c.active.as_ref().map(|a| a.tx.clone())
    };
    tracing::debug!(sessionTransport = "websocket", code = ?error_code(&error), active = active.is_some(), "session websocket poisoned");
    if let Some(tx) = active {
        let _ = tx.send(Err(error));
        stream_failure(owner);
    }
    if let Some(reader) = channel.reader.lock().take() {
        reader.abort();
    }
    channel.connection.close();
}

/// Closes the channel when the exchange start is abandoned midway (`Effect.onInterrupt`).
struct CloseOnDrop<'a> {
    owner: &'a Arc<Owner>,
    channel: &'a Arc<Channel>,
    armed: bool,
}

impl Drop for CloseOnDrop<'_> {
    fn drop(&mut self) {
        if self.armed {
            close_channel(self.owner, self.channel);
        }
    }
}

/// The end of one exchange (`Stream.ensuring` in `start`): success keeps the socket (closing it after
/// an error frame), anything else drops the staged checkpoint and poisons the socket.
struct ExchangeGuard {
    owner: Arc<Owner>,
    channel: Arc<Channel>,
    active: u64,
    delivery: Arc<AtomicU8>,
    rx: mpsc::UnboundedReceiver<Result<String, AIError>>,
    terminal: Option<&'static str>,
    url: String,
    done: bool,
}

impl ExchangeGuard {
    fn ensure(&mut self) {
        if self.done {
            return;
        }
        self.done = true;
        {
            let mut c = self.channel.state.lock();
            if c.active.as_ref().is_some_and(|a| a.id == self.active) {
                c.active = None;
            }
        }
        let pending = self.rx.len();
        self.rx.close();
        if let Some(terminal) = self.terminal
            && pending == 0
        {
            self.owner.state.lock().stream_failures = 0;
            tracing::debug!(sessionTransport = "websocket", r#type = terminal, "session websocket exchange ended");
            // The Codex backend stops serving a connection after any error frame: the next request is never
            // answered and the socket dies with 1006. api.openai.com keeps it open, so reconnecting costs one
            // handshake there. Drop the socket after every error so retries never race that.
            if terminal != "completed" && terminal != "incomplete" {
                close_channel(&self.owner, &self.channel);
            }
            return;
        }
        {
            let mut c = self.channel.state.lock();
            c.checkpoint = None;
            c.pending = None;
        }
        let error = if self.terminal.is_some() {
            transport_error(
                "WebSocket data arrived after the terminal event",
                TransportOperation::Read,
                WsError { url: Some(&self.url), code: Some("idle-data"), phase: Some("receive"), delivery: Some("accepted"), ..Default::default() },
            )
        } else {
            let delivery = if self.delivery.load(Ordering::SeqCst) == PROVIDER_OBSERVED { "accepted" } else { "ambiguous" };
            transport_error(
                "Session WebSocket exchange did not reach a terminal event",
                TransportOperation::Read,
                WsError { url: Some(&self.url), code: Some("incomplete"), phase: Some("receive"), delivery: Some(delivery), ..Default::default() },
            )
        };
        poison(&self.owner, &self.channel, error);
    }
}

impl Drop for ExchangeGuard {
    fn drop(&mut self) {
        self.ensure();
    }
}

/// What `start` hands back: the frames and the success acknowledgement.
struct Started {
    frames: FrameStream,
    complete: Box<dyn FnOnce() + Send>,
}

fn fallback(exchange: ChannelExchange) -> Started {
    Started { frames: (exchange.fallback)(), complete: Box::new(|| {}) }
}

/// A rejected oversized request (close 1009 before the provider answered) moves the session to HTTP.
fn too_large(error: &AIError) -> bool {
    error_code(error) == Some("1009") && error_delivery(error) == Some("rejected")
}

impl SessionModelTransport {
    pub fn new(connector: Arc<dyn WebSocketConnector>) -> Self {
        Self { connector, states: Mutex::new(HashMap::new()), ids: AtomicU64::new(1) }
    }

    fn state(&self, session_id: &str) -> Arc<Owner> {
        self.states
            .lock()
            .entry(session_id.to_string())
            .or_insert_with(|| Arc::new(Owner { lock: Arc::new(tokio::sync::Mutex::new(())), state: Mutex::new(OwnerState::default()) }))
            .clone()
    }

    /// `bind`: the channel executor of one session.
    pub fn bind(self: &Arc<Self>, session_id: &str, interceptor: Option<Arc<dyn Interceptor>>, idle: IdleTimeout) -> Arc<dyn ChannelExecutor> {
        Arc::new(Bound { transport: self.clone(), session_id: session_id.to_string(), interceptor, idle })
    }

    /// `close`: forget the session and close its socket.
    pub fn close(&self, session_id: &str) {
        let Some(owner) = self.states.lock().remove(session_id) else { return };
        let channel = {
            let mut o = owner.state.lock();
            o.closed = true;
            o.channel.clone()
        };
        if let Some(channel) = channel {
            close_channel(&owner, &channel);
        }
    }

    /// `close` for every session matching `filter` (a closing location's sessions).
    pub fn close_where(&self, filter: impl Fn(&str) -> bool) {
        let ids: Vec<String> = self.states.lock().keys().filter(|id| filter(id)).cloned().collect();
        for id in ids {
            self.close(&id);
        }
    }

    /// `closeAll`.
    pub fn close_all(&self) {
        let owners: Vec<Arc<Owner>> = self.states.lock().drain().map(|(_, o)| o).collect();
        for owner in owners {
            let channel = {
                let mut o = owner.state.lock();
                o.closed = true;
                o.channel.clone()
            };
            if let Some(channel) = channel {
                close_channel(&owner, &channel);
            }
        }
    }

    /// `closeAll` at shutdown: close every socket and wait until the close frames went out.
    pub async fn shutdown(&self) {
        let owners: Vec<Arc<Owner>> = self.states.lock().drain().map(|(_, o)| o).collect();
        for owner in owners {
            let channel = {
                let mut o = owner.state.lock();
                o.closed = true;
                o.channel.clone()
            };
            if let Some(channel) = channel
                && release_channel(&owner, &channel)
            {
                channel.connection.close_and_wait().await;
            }
        }
    }

    /// Whether the session has an open socket (tests, diagnostics).
    pub fn connected(&self, session_id: &str) -> bool {
        self.states.lock().get(session_id).is_some_and(|o| o.state.lock().channel.is_some())
    }

    async fn open(&self, owner: &Arc<Owner>, url: &str, headers: &Headers, key: String) -> Result<Arc<Channel>, AIError> {
        let connection = match tokio::time::timeout(CONNECT_TIMEOUT, self.connector.open(url, headers)).await {
            Ok(result) => result?,
            Err(_) => {
                return Err(transport_error(
                    "Timed out opening the Session WebSocket",
                    TransportOperation::Request,
                    WsError { url: Some(url), code: Some("connect-timeout"), phase: Some("connect"), delivery: Some("not-sent"), ..Default::default() },
                ));
            }
        };
        if owner.state.lock().closed {
            connection.close();
            return Err(transport_error(
                "Session WebSocket owner closed while connecting",
                TransportOperation::Request,
                WsError { code: Some("owner-closed"), phase: Some("connect"), delivery: Some("not-sent"), ..Default::default() },
            ));
        }
        let messages = connection.take_messages();
        let channel = Arc::new(Channel {
            affinity: key,
            connection,
            opened_at: Instant::now(),
            state: Mutex::new(ChannelState::default()),
            reader: Mutex::new(None),
        });
        owner.state.lock().channel = Some(channel.clone());
        if let Some(mut messages) = messages {
            let (reader_owner, reader_channel, reader_url) = (owner.clone(), channel.clone(), url.to_string());
            let reader = tokio::spawn(async move {
                let (owner, channel, url) = (reader_owner, reader_channel, reader_url);
                while let Some(message) = messages.recv().await {
                    let failure = match message {
                        Ok(message) => {
                            let c = channel.state.lock();
                            match &c.active {
                                None => Some(transport_error(
                                    "WebSocket data arrived without an active exchange",
                                    TransportOperation::Read,
                                    WsError { url: Some(&url), code: Some("idle-data"), phase: Some("receive"), ..Default::default() },
                                )),
                                Some(active) => {
                                    active.delivery.store(PROVIDER_OBSERVED, Ordering::SeqCst);
                                    match message {
                                        WsMessage::Text(text) => {
                                            let _ = active.tx.send(Ok(text));
                                            None
                                        }
                                        WsMessage::Binary(_) => Some(transport_error(
                                            "Unsupported binary WebSocket frame",
                                            TransportOperation::Read,
                                            WsError { url: Some(&url), code: Some("message"), phase: Some("receive"), ..Default::default() },
                                        )),
                                    }
                                }
                            }
                        }
                        Err(error) => Some(error),
                    };
                    let Some(error) = failure else { continue };
                    let (closing, delivery) = {
                        let c = channel.state.lock();
                        (c.closing, c.active.as_ref().map(|a| a.delivery.load(Ordering::SeqCst)))
                    };
                    if closing {
                        return;
                    }
                    let phase = if error_phase(&error) == Some("close") { "close" } else { "receive" };
                    let delivery = if delivery.is_some_and(accepted) {
                        "accepted"
                    } else if error_code(&error) == Some("1009") {
                        "rejected"
                    } else {
                        "ambiguous"
                    };
                    poison(&owner, &channel, annotate(error, phase, delivery));
                    return;
                }
            });
            *channel.reader.lock() = Some(reader.abort_handle());
        }
        tracing::debug!(sessionTransport = "websocket", phase = "connect", "session websocket connected");
        Ok(channel)
    }

    async fn start(
        &self,
        owner: &Arc<Owner>,
        mut exchange: ChannelExchange,
        interceptor: Option<Arc<dyn Interceptor>>,
        idle: IdleTimeout,
    ) -> Result<Started, AIError> {
        {
            let o = owner.state.lock();
            if o.closed {
                return Err(transport_error(
                    "Session WebSocket owner is closed",
                    TransportOperation::Request,
                    WsError { code: Some("owner-closed"), phase: Some("queue"), delivery: Some("not-sent"), ..Default::default() },
                ));
            }
            if o.http_fallback {
                drop(o);
                return Ok(fallback(exchange));
            }
        }
        if let Some(i) = &interceptor {
            let (url, headers) = i
                .handshake(exchange.connect.url.clone(), exchange.connect.headers.clone())
                .await
                .map_err(|m| transport_error(m, TransportOperation::Request, WsError { phase: Some("connect"), delivery: Some("not-sent"), ..Default::default() }))?;
            exchange.connect.url = url;
            exchange.connect.headers = headers.into_iter().map(|(k, v)| (k.to_ascii_lowercase(), v)).collect();
        }
        let url = exchange.connect.url.clone();
        let key = affinity(&url, &exchange.connect.headers);
        let current = owner.state.lock().channel.clone();
        let rotate_after = Duration::from_millis(exchange.connect.rotate_after_ms.unwrap_or(ROTATE_AFTER_MS));
        if let Some(current) = &current {
            let rotation = if current.affinity != key {
                Some("affinity")
            } else if current.opened_at.elapsed() >= rotate_after {
                Some("age")
            } else {
                None
            };
            if let Some(reason) = rotation {
                tracing::debug!(sessionTransport = "websocket", phase = "connect", reason, "session websocket rotating");
                close_channel(owner, current);
            }
        }
        let existing = owner.state.lock().channel.clone();
        let channel = match existing {
            Some(channel) => {
                tracing::debug!(sessionTransport = "websocket", phase = "connect", "session websocket reused");
                channel
            }
            None => match self.open(owner, &url, &exchange.connect.headers, key).await {
                Ok(channel) => channel,
                Err(error) if error_code(&error) == Some("owner-closed") => return Err(error),
                Err(error) => {
                    // Any connect failure, transient or not, pins the session to HTTP until restart or move:
                    // a network that refuses the upgrade would otherwise charge every step for a failed connect.
                    owner.state.lock().http_fallback = true;
                    tracing::warn!(sessionTransport = "websocket", phase = "connect", delivery = "not-sent", code = ?error_code(&error), error = %error.message, "session websocket connect failed; using http");
                    return Ok(fallback(exchange));
                }
            },
        };
        let mut abandon = CloseOnDrop { owner, channel: &channel, armed: true };
        let checkpoint = {
            let mut c = channel.state.lock();
            if c.pending.is_some() {
                c.pending = None;
                c.checkpoint = None;
            }
            c.checkpoint.clone()
        };
        let create = match exchange.driver.create(checkpoint.as_ref()) {
            Ok(create) => create,
            Err(error) => return Err(error),
        };
        if create.mode == ChannelMode::Full {
            channel.state.lock().checkpoint = None;
        }
        let message = match &interceptor {
            Some(i) => i
                .send(create.message.clone())
                .await
                .map_err(|m| transport_error(m, TransportOperation::Write, WsError { phase: Some("send"), delivery: Some("not-sent"), ..Default::default() }))?,
            None => create.message.clone(),
        };
        tracing::debug!(sessionTransport = "websocket", phase = "send", mode = create.mode.as_str(), "session websocket sending");
        let (tx, rx) = mpsc::unbounded_channel();
        let delivery = Arc::new(AtomicU8::new(SEND_ATTEMPTED));
        let active = self.ids.fetch_add(1, Ordering::SeqCst);
        channel.state.lock().active = Some(Active { id: active, tx, delivery: delivery.clone() });
        if let Err(failure) = channel.connection.send_text(message).await {
            abandon.armed = false;
            let not_sent = error_delivery(&failure) == Some("not-sent");
            close_channel(owner, &channel);
            if not_sent {
                return Ok(fallback(exchange));
            }
            stream_failure(owner);
            return Err(annotate(failure, "send", "ambiguous"));
        }
        abandon.armed = false;
        drop(abandon);

        let token = self.ids.fetch_add(1, Ordering::SeqCst);
        let guard = ExchangeGuard { owner: owner.clone(), channel: channel.clone(), active, delivery: delivery.clone(), rx, terminal: None, url: url.clone(), done: false };
        let ChannelExchange { fallback: http_fallback, mut driver, .. } = exchange;
        let (frame_owner, frame_channel) = (owner.clone(), channel.clone());
        let frames = async_stream::stream! {
            let mut guard = guard;
            let mut http_fallback = Some(http_fallback);
            let (owner, channel) = (frame_owner, frame_channel);
            loop {
                let next = match idle.duration() {
                    Some(limit) => tokio::time::timeout(limit, guard.rx.recv()).await.ok(),
                    None => Some(guard.rx.recv().await),
                };
                let item = match next {
                    None => Err(transport_error(
                        "Timed out waiting for WebSocket data",
                        TransportOperation::Read,
                        WsError {
                            url: Some(&url),
                            code: Some("idle-timeout"),
                            phase: Some("receive"),
                            delivery: Some(if delivery.load(Ordering::SeqCst) == PROVIDER_OBSERVED { "accepted" } else { "ambiguous" }),
                            ..Default::default()
                        },
                    )),
                    Some(None) => break,
                    Some(Some(item)) => item,
                };
                let frame = match item {
                    Ok(frame) => match &interceptor {
                        Some(i) => i.receive(frame).await.map_err(|m| transport_error(m, TransportOperation::Read, WsError { phase: Some("receive"), ..Default::default() })),
                        None => Ok(frame),
                    },
                    Err(error) => Err(error),
                };
                let observation = frame.and_then(|frame| driver.observe(&create, &frame));
                let observation = match observation {
                    Ok(observation) => observation,
                    Err(error) => {
                        guard.ensure();
                        if too_large(&error) {
                            owner.state.lock().http_fallback = true;
                            tracing::warn!(sessionTransport = "websocket", phase = "close", delivery = "rejected", "session websocket request too large; using http");
                            if let Some(f) = http_fallback.take() {
                                let mut frames = f();
                                while let Some(item) = frames.next().await {
                                    yield item;
                                }
                            }
                            return;
                        }
                        yield Err(error);
                        return;
                    }
                };
                let terminal = observation.is_terminal();
                if terminal {
                    guard.terminal = Some(observation.type_name());
                    delivery.store(TERMINAL, Ordering::SeqCst);
                    let staged = match &observation {
                        ChannelObservation::Completed { checkpoint: Some(checkpoint), .. } => Some(checkpoint.clone()),
                        _ => None,
                    };
                    let mut c = channel.state.lock();
                    match staged {
                        Some(checkpoint) => c.pending = Some((token, checkpoint)),
                        None => c.checkpoint = None,
                    }
                }
                match observation.into_frame() {
                    Ok(frame) => {
                        yield Ok(frame);
                        if terminal {
                            guard.ensure();
                            return;
                        }
                    }
                    Err(error) => {
                        guard.ensure();
                        yield Err(error);
                        return;
                    }
                }
            }
            guard.ensure();
        };
        let (done_owner, done_channel) = (owner.clone(), channel.clone());
        let complete = Box::new(move || {
            let o = done_owner.state.lock();
            if !same(&o.channel, &done_channel) {
                return;
            }
            let mut c = done_channel.state.lock();
            if c.pending.as_ref().is_some_and(|(t, _)| *t == token) {
                c.checkpoint = c.pending.take().map(|(_, checkpoint)| checkpoint);
            }
        });
        Ok(Started { frames: Box::pin(frames), complete })
    }
}

struct Bound {
    transport: Arc<SessionModelTransport>,
    session_id: String,
    interceptor: Option<Arc<dyn Interceptor>>,
    idle: IdleTimeout,
}

impl ChannelExecutor for Bound {
    fn execute(&self, exchange: ChannelExchange) -> ChannelExecution {
        let owner = self.transport.state(&self.session_id);
        let transport = self.transport.clone();
        let (interceptor, idle) = (self.interceptor.clone(), self.idle);
        let completion: Arc<Mutex<Option<Box<dyn FnOnce() + Send>>>> = Arc::default();
        let slot = completion.clone();
        let frames = async_stream::stream! {
            // Exchanges of one session run one at a time; the lock is held until the frames end.
            let _lock = owner.lock.clone().lock_owned().await;
            match transport.start(&owner, exchange, interceptor, idle).await {
                Ok(started) => {
                    *slot.lock() = Some(started.complete);
                    let mut frames = started.frames;
                    while let Some(item) = frames.next().await {
                        yield item;
                    }
                }
                Err(error) => yield Err(error),
            }
        };
        ChannelExecution {
            frames: Box::pin(frames),
            complete: Box::new(move || {
                if let Some(complete) = completion.lock().take() {
                    complete();
                }
            }),
        }
    }
}
