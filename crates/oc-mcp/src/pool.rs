//! MCP connection pooling (`packages/core/src/mcp/pool.ts`, `McpPool`): one connection to a server shared by every
//! Location in this process that configures it identically (`pool: true`).
//!
//! Each start joins the connection other Locations hold under the same key, or opens it, and each stop lets go of
//! it; the last to let go closes it. A server that negotiates the legacy protocol, or that answers but refuses or
//! breaks the pooled handshake, is never shared: the Location connects on its own, exactly as it would unpooled, and
//! every later start for that key skips straight to that.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Weak};

use async_trait::async_trait;
use futures::FutureExt;
use futures::future::BoxFuture;
use parking_lot::Mutex;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use tokio::sync::watch;
use tokio_util::sync::CancellationToken;

use crate::config::{Protocol, ServerConfig};
use crate::connection::Connection;
use crate::error::McpError;
use crate::link::{AuthProvider, Kind, Link, LinkEvents, LinkInput, Listed, Lock, Spawn};
use crate::protocol::ElicitationHandler;
use crate::types::{ClientInfo, Status};

/// What one start selected: what tells this connection apart from every other one to the same server (see
/// [`key`]), and the OAuth provider to open it with. Both come from one credential selection, so a slot's key and the
/// account it connects as can't disagree.
#[derive(Clone)]
pub struct Selection {
    pub key: String,
    pub auth_provider: AuthProvider,
}

/// Runs once per start (`select`).
pub type Select = Arc<dyn Fn() -> BoxFuture<'static, Selection> + Send + Sync>;

/// What [`key`] reads.
pub struct KeyInput<'a> {
    pub config: &'a ServerConfig,
    /// Local only: the directory the server runs in, as [`crate::connection::resolve_cwd`] resolves it for the spawn.
    pub cwd: Option<&'a std::path::Path>,
    /// The active OAuth credential's ID, which a token refresh keeps and a sign-in replaces.
    pub credential: Option<&'a str>,
    pub client_info: Option<&'a ClientInfo>,
}

/// `SharedCache.fingerprint`: never a raw secret in a key.
fn fingerprint(secret: &str) -> String {
    hex::encode(Sha256::digest(secret.as_bytes()))[..16].to_string()
}

fn fingerprint_map(map: Option<&indexmap::IndexMap<String, String>>) -> Value {
    match map {
        None => Value::Null,
        Some(map) => {
            let mut entries: Vec<(&String, &String)> = map.iter().collect();
            entries.sort_by(|a, b| crate::manager::locale_cmp(a.0, b.0));
            let pairs: Vec<Value> = entries.into_iter().map(|(k, v)| json!([k, v])).collect();
            json!(fingerprint(&Value::Array(pairs).to_string()))
        }
    }
}

/// `McpPool.key`: everything that could make two connections behave differently, so only identical ones are shared.
/// The server's name is left out; each Location keeps its own. Secrets appear only as fingerprints.
pub fn key(input: &KeyInput<'_>) -> String {
    let config = input.config;
    let protocol = config.protocol().map(|p| json!(p.as_str())).unwrap_or(Value::Null);
    let timeout = config.timeout().map(|t| serde_json::to_value(t).unwrap_or(Value::Null)).unwrap_or(Value::Null);
    let client_info = input.client_info.map(|c| serde_json::to_value(c).unwrap_or(Value::Null)).unwrap_or(Value::Null);
    let common = [protocol, timeout, client_info];
    let mut parts = match config {
        ServerConfig::Remote(remote) => {
            let oauth = match &remote.oauth {
                None => Value::Null,
                Some(crate::config::OAuthSetting::Disabled) => json!(false),
                Some(crate::config::OAuthSetting::Config(oauth)) => {
                    let mut value = serde_json::to_value(oauth).unwrap_or(Value::Null);
                    value["client_secret"] = oauth.client_secret.as_deref().map(|s| json!(fingerprint(s))).unwrap_or(Value::Null);
                    value
                }
            };
            vec![
                json!("remote"),
                json!(remote.url),
                fingerprint_map(remote.headers.as_ref()),
                oauth,
                // A remote server's URL gains ?codemode=false unless Code Mode is off.
                json!(remote.codemode != Some(false)),
                input.credential.map(|c| json!(c)).unwrap_or(Value::Null),
            ]
        }
        // Only Locations on this host share local servers, so the execution plane is always the host.
        ServerConfig::Local(local) => vec![
            json!("local"),
            json!("host"),
            json!(local.command),
            input.cwd.map(|c| json!(c.display().to_string())).unwrap_or(Value::Null),
            fingerprint_map(local.environment.as_ref()),
        ],
    };
    parts.extend(common);
    Value::Array(parts).to_string()
}

/// A Location holding a shared connection. The pool reaches Locations only through the holders registered right
/// now, never through the one that opened the connection.
pub struct Holder {
    auth_provider: AuthProvider,
    elicitation: Arc<dyn ElicitationHandler>,
    closed: Box<dyn Fn(String) + Send + Sync>,
    changed: Box<dyn Fn(Kind) + Send + Sync>,
}

#[derive(Clone)]
enum Outcome {
    Connected,
    Private,
    Failed(Status),
}

struct Slot {
    key: String,
    name: String,
    link: std::sync::OnceLock<Arc<Link>>,
    holders: Mutex<Vec<Arc<Holder>>>,
    lock: Lock,
    outcome: watch::Sender<Option<Outcome>>,
    started: AtomicBool,
    connected: AtomicBool,
}

impl Slot {
    fn link(&self) -> &Arc<Link> {
        self.link.get().expect("slot link")
    }

    /// `Deferred.succeed`: the first outcome wins.
    fn settle(&self, outcome: Outcome) {
        self.outcome.send_if_modified(|current| {
            if current.is_some() {
                return false;
            }
            *current = Some(outcome);
            true
        });
    }

    fn holders(&self) -> Vec<Arc<Holder>> {
        self.holders.lock().clone()
    }
}

#[derive(Default)]
struct Inner {
    slots: Mutex<HashMap<String, Arc<Slot>>>,
    /// Keys whose server negotiated legacy or refused the pooled handshake. Kept for the life of the process.
    unpoolable: Mutex<HashSet<String>>,
    /// The subset that negotiated legacy. Their private connections skip the 2026-07-28 probe; a refusal proves
    /// nothing about the protocol, so those keep the configured one.
    legacy: Mutex<HashSet<String>>,
    /// Names of local servers already warned about for having no cwd, which keeps each project's folder in the key.
    unfixed: Mutex<HashSet<String>>,
}

/// The process-wide pool (`McpPool.Service`, a global node). Cheap to clone.
#[derive(Clone, Default)]
pub struct McpPool {
    inner: Arc<Inner>,
}

/// What a shared connection takes from the Location that opens it: only data, since the connection outlives that
/// Location's claim.
struct Opening {
    name: String,
    config: ServerConfig,
    directory: PathBuf,
    client_info: ClientInfo,
}

impl McpPool {
    pub fn new() -> McpPool {
        McpPool::default()
    }

    fn unpoolable(&self, key: &str) -> bool {
        self.inner.unpoolable.lock().contains(key)
    }

    fn detach(&self, key: &str, slot: &Arc<Slot>) {
        let mut slots = self.inner.slots.lock();
        if slots.get(key).is_some_and(|s| Arc::ptr_eq(s, slot)) {
            slots.remove(key);
        }
    }

    /// How many shared connections are open or opening (tests).
    pub fn open_slots(&self) -> usize {
        self.inner.slots.lock().len()
    }

    fn open(&self, key: &str, input: Opening) -> Arc<Slot> {
        let slot = Arc::new(Slot {
            key: key.to_string(),
            name: input.name.clone(),
            link: std::sync::OnceLock::new(),
            holders: Mutex::new(Vec::new()),
            lock: Lock::default(),
            outcome: watch::channel(None).0,
            started: AtomicBool::new(false),
            connected: AtomicBool::new(false),
        });
        let weak = Arc::downgrade(&slot);
        // Every holder of a key presents the same credential, so any of them can build the provider.
        let auth: AuthProvider = {
            let weak = weak.clone();
            Arc::new(move || {
                let first = weak.upgrade().and_then(|slot| slot.holders.lock().first().map(|h| h.auth_provider.clone()));
                match first {
                    Some(provider) => provider(),
                    None => Box::pin(async { Ok(None) }),
                }
            })
        };
        let link = Link::new(LinkInput {
            name: input.name,
            config: input.config,
            directory: input.directory,
            shared: true,
            client_info: input.client_info,
            auth_provider: auth,
            elicitation: Some(Arc::new(Shared { slot: weak.clone() })),
            lock: slot.lock.clone(),
            callbacks: Arc::new(Spawn),
            fork: Arc::new(Spawn),
            events: Arc::new(SlotEvents { pool: Arc::downgrade(&self.inner), slot: weak }),
        });
        let _ = slot.link.set(link);
        self.inner.slots.lock().insert(key.to_string(), slot.clone());
        slot
    }

    /// Starts the shared attempt: once, and once a holder is registered, since it reads its OAuth provider from the
    /// holders.
    fn connect(&self, slot: &Arc<Slot>) {
        if slot.started.swap(true, Ordering::SeqCst) {
            return;
        }
        let (pool, slot) = (self.clone(), slot.clone());
        tokio::spawn(async move {
            let attempt = std::panic::AssertUnwindSafe(async {
                let _guard = slot.lock.lock().await;
                slot.link().start().await;
                // A failed or legacy start already settled the outcome.
                if slot.link().client().is_none() {
                    return;
                }
                slot.connected.store(true, Ordering::SeqCst);
                slot.settle(Outcome::Connected);
            })
            .catch_unwind()
            .await;
            // Holders wait on the outcome, so an attempt that dies still settles it.
            if let Err(panic) = attempt {
                pool.detach(&slot.key, &slot);
                let error = panic
                    .downcast_ref::<String>()
                    .cloned()
                    .or_else(|| panic.downcast_ref::<&str>().map(|s| s.to_string()))
                    .unwrap_or_else(|| "MCP pool attempt failed".to_string());
                slot.settle(Outcome::Failed(Status::Failed { error }));
            }
        });
    }

    /// Lets go of a claim. The last holder to let go closes the connection; a start nobody waits for any more
    /// finishes first, under the slot's lock, and is closed after.
    async fn release(&self, claim: Claim) {
        let Claim { slot, holder } = claim;
        let last = {
            let mut holders = slot.holders.lock();
            holders.retain(|h| !Arc::ptr_eq(h, &holder));
            holders.is_empty()
        };
        if !last {
            return;
        }
        self.detach(&slot.key, &slot);
        let _guard = slot.lock.lock().await;
        slot.link().stop().await;
    }

    /// Registers `holder` on the key's slot, opening it if needed, and starts its attempt. `None` when the key can't
    /// be shared.
    fn acquire(&self, key: &str, input: Opening, holder: &Arc<Holder>) -> Option<Claim> {
        if self.unpoolable(key) {
            return None;
        }
        let existing = self.inner.slots.lock().get(key).cloned();
        let slot = existing.unwrap_or_else(|| self.open(key, input));
        slot.holders.lock().push(holder.clone());
        self.connect(&slot);
        Some(Claim { slot, holder: holder.clone() })
    }

    /// A Location's connection to a server that may be shared (`connection`).
    pub fn connection(&self, input: PoolInput) -> Arc<Pooled> {
        Pooled::new(self.clone(), input)
    }
}

/// A Location's hold on a shared slot.
struct Claim {
    slot: Arc<Slot>,
    holder: Arc<Holder>,
}

impl Slot {
    async fn outcome(&self) -> Outcome {
        let mut outcome = self.outcome.subscribe();
        let settled = outcome.wait_for(Option::is_some).await.map(|o| o.clone());
        match settled {
            Ok(Some(outcome)) => outcome,
            _ => Outcome::Failed(Status::failed("MCP pool closed")),
        }
    }
}

/// The shared connection's events.
struct SlotEvents {
    pool: Weak<Inner>,
    slot: Weak<Slot>,
}

impl SlotEvents {
    fn parts(&self) -> Option<(McpPool, Arc<Slot>)> {
        Some((McpPool { inner: self.pool.upgrade()? }, self.slot.upgrade()?))
    }
}

#[async_trait]
impl LinkEvents for SlotEvents {
    async fn connecting(&self) {}
    async fn connected(&self) {}

    /// A server that answered but refused the pooled handshake, such as one that wants roots, may still accept the
    /// Location's own connection; anything unreachable would only fail again.
    async fn failed(&self, failure: Status, answered: bool) {
        let Some((pool, slot)) = self.parts() else { return };
        pool.detach(&slot.key, &slot);
        if answered {
            pool.inner.unpoolable.lock().insert(slot.key.clone());
            let error = match &failure {
                Status::Failed { error } | Status::NeedsAuth { error } => error.clone(),
                _ => String::new(),
            };
            tracing::warn!(server = %slot.name, error = %error, "mcp pool ignored: server refused the shared connection");
        }
        slot.settle(if answered { Outcome::Private } else { Outcome::Failed(failure) });
    }

    async fn legacy(&self, version: Option<String>) {
        let Some((pool, slot)) = self.parts() else { return };
        pool.detach(&slot.key, &slot);
        pool.inner.unpoolable.lock().insert(slot.key.clone());
        pool.inner.legacy.lock().insert(slot.key.clone());
        tracing::warn!(server = %slot.name, version = ?version, "mcp pool ignored: server speaks an older protocol");
        slot.settle(Outcome::Private);
    }

    async fn settled(&self) {}

    async fn closed(&self, reason: String) {
        let Some((pool, slot)) = self.parts() else { return };
        pool.detach(&slot.key, &slot);
        let holders: Vec<Arc<Holder>> = std::mem::take(&mut *slot.holders.lock());
        slot.link().stop().await;
        for holder in holders {
            (holder.closed)(reason.clone());
        }
    }

    async fn changed(&self, kind: Kind) {
        let Some(slot) = self.slot.upgrade() else { return };
        if slot.connected.load(Ordering::SeqCst) {
            for holder in slot.holders() {
                (holder.changed)(kind);
            }
        }
    }
}

/// The shared connection's own elicitation handler. Elicitations a request carries reach that request's Location
/// (see [`Pooled::recovering`]). Any other one, which the server could not tie to a request, goes to every holder
/// and the first answer wins; the others are dropped, which cancels their forms.
struct Shared {
    slot: Weak<Slot>,
}

#[async_trait]
impl ElicitationHandler for Shared {
    async fn create(&self, server: &str, params: Value, cancel: CancellationToken) -> Result<Value, String> {
        let holders = self.slot.upgrade().map(|slot| slot.holders()).unwrap_or_default();
        if holders.is_empty() {
            return Ok(json!({ "action": "cancel" }));
        }
        let asks = holders.into_iter().map(|holder| {
            let (server, params, cancel) = (server.to_string(), params.clone(), cancel.clone());
            async move { holder.elicitation.create(&server, params, cancel).await }.boxed()
        });
        let (result, _, rest) = futures::future::select_all(asks).await;
        drop(rest);
        result
    }

    async fn complete(&self, _server: &str, _elicitation_id: &str) {}
}

/// A Location's elicitation handler for requests through a shared connection: the connection carries the name of
/// the Location that opened it; forms show this Location's.
struct Named {
    name: String,
    elicitation: Arc<dyn ElicitationHandler>,
}

#[async_trait]
impl ElicitationHandler for Named {
    async fn create(&self, _server: &str, params: Value, cancel: CancellationToken) -> Result<Value, String> {
        self.elicitation.create(&self.name, params, cancel).await
    }

    async fn complete(&self, server: &str, elicitation_id: &str) {
        self.elicitation.complete(server, elicitation_id).await
    }
}

/// `McpPool.Input`: a Location's connection input plus `select`.
#[derive(Clone)]
pub struct PoolInput {
    pub link: LinkInput,
    pub select: Select,
}

#[derive(Default)]
struct PooledState {
    claim: Option<Claim>,
    holder: Option<Arc<Holder>>,
    shared: Option<Arc<Link>>,
    own: Option<Arc<Link>>,
    /// This start's credential selection, which a private connection opens with as well.
    selected: Option<Selection>,
}

/// A Location's connection to a pooled server: the shared connection while it holds one, else its own.
pub struct Pooled {
    pool: McpPool,
    input: LinkInput,
    select: Select,
    /// A private connection, used when the key can't be shared, with the configured protocol or plain legacy.
    configured: Arc<Link>,
    older: Arc<Link>,
    elicit: Option<Arc<dyn ElicitationHandler>>,
    state: Mutex<PooledState>,
    this: Weak<Pooled>,
}

impl Pooled {
    fn new(pool: McpPool, input: PoolInput) -> Arc<Pooled> {
        Arc::new_cyclic(|this: &Weak<Pooled>| {
            let private = |config: ServerConfig| {
                let weak = this.clone();
                let fallback = input.link.auth_provider.clone();
                let auth: AuthProvider = Arc::new(move || {
                    let selected = weak.upgrade().and_then(|p| p.state.lock().selected.as_ref().map(|s| s.auth_provider.clone()));
                    selected.unwrap_or_else(|| fallback.clone())()
                });
                Link::new(LinkInput { config, auth_provider: auth, ..input.link.clone() })
            };
            let mut older = input.link.config.clone();
            older.set_protocol(Some(Protocol::Legacy));
            Pooled {
                configured: private(input.link.config.clone()),
                older: private(older),
                elicit: input.link.elicitation.clone().map(|elicitation| {
                    Arc::new(Named { name: input.link.name.clone(), elicitation }) as Arc<dyn ElicitationHandler>
                }),
                pool,
                select: input.select,
                input: input.link,
                state: Mutex::new(PooledState::default()),
                this: this.clone(),
            }
        })
    }

    fn live(&self) -> Arc<Link> {
        let state = self.state.lock();
        state.shared.clone().or_else(|| state.own.clone()).unwrap_or_else(|| self.configured.clone())
    }

    fn private(&self) -> Arc<Link> {
        self.state.lock().own.clone().unwrap_or_else(|| self.configured.clone())
    }

    pub fn client(&self) -> Option<Arc<Connection>> {
        self.live().client()
    }

    pub fn tools(&self) -> Option<Listed> {
        self.live().tools()
    }

    pub fn prompts(&self) -> Option<Listed> {
        self.live().prompts()
    }

    /// Holder callbacks run in the Location's own callback runtime, under its lock, and only while the claim they
    /// belong to is the current one.
    fn current(&self, holder: &Weak<Holder>, work: impl FnOnce(Arc<dyn LinkEvents>) -> BoxFuture<'static, ()> + Send + 'static) {
        let (Some(this), holder) = (self.this.upgrade(), holder.clone()) else { return };
        let lock = self.input.lock.clone();
        self.input.callbacks.run(Box::pin(async move {
            let _guard = lock.lock().await;
            let current = this.state.lock().holder.as_ref().is_some_and(|h| holder.upgrade().is_some_and(|w| Arc::ptr_eq(h, &w)));
            if current {
                work(this.input.events.clone()).await;
            }
        }));
    }

    async fn clear(&self) {
        let claim = {
            let mut state = self.state.lock();
            state.holder = None;
            state.shared = None;
            state.claim.take()
        };
        if let Some(claim) = claim {
            self.pool.release(claim).await;
        }
    }

    /// Resolves false when the key can't be shared, leaving the start to the Location's own connection.
    async fn join(&self, selected: &Selection) -> bool {
        let events = self.input.events.clone();
        events.connecting().await;
        let holder = Arc::new_cyclic(|weak: &Weak<Holder>| {
            let (closer, changer) = (self.this.clone(), self.this.clone());
            let (closed_holder, changed_holder) = (weak.clone(), weak.clone());
            Holder {
                auth_provider: selected.auth_provider.clone(),
                elicitation: self.elicit.clone().unwrap_or_else(|| Arc::new(Cancel)),
                closed: Box::new(move |reason| {
                    if let Some(this) = closer.upgrade() {
                        this.current(&closed_holder, move |events| Box::pin(async move { events.closed(reason).await }));
                    }
                }),
                changed: Box::new(move |kind| {
                    if let Some(this) = changer.upgrade() {
                        this.current(&changed_holder, move |events| Box::pin(async move { events.changed(kind).await }));
                    }
                }),
            }
        });
        let opening = Opening {
            name: self.input.name.clone(),
            config: self.input.config.clone(),
            directory: self.input.directory.clone(),
            client_info: self.input.client_info.clone(),
        };
        let Some(claim) = self.pool.acquire(&selected.key, opening, &holder) else { return false };
        let slot = claim.slot.clone();
        {
            let mut state = self.state.lock();
            state.claim = Some(claim);
            state.holder = Some(holder);
        }
        match slot.outcome().await {
            Outcome::Private => {
                self.clear().await;
                false
            }
            Outcome::Failed(failure) => {
                self.clear().await;
                events.failed(failure, false).await;
                events.settled().await;
                true
            }
            Outcome::Connected => {
                self.state.lock().shared = Some(slot.link().clone());
                events.connected().await;
                events.settled().await;
                true
            }
        }
    }

    /// Joins the shared connection, or connects on its own. The caller holds the Location's lock.
    pub fn start(&self) -> BoxFuture<'_, ()> {
        Box::pin(async move {
            if let ServerConfig::Local(local) = &self.input.config
                && local.cwd.is_none()
                && self.pool.inner.unfixed.lock().insert(self.input.name.clone())
            {
                tracing::warn!(server = %self.input.name, "mcp pool ignored: local server has no fixed cwd, so each directory runs its own");
            }
            let selected = (self.select)().await;
            self.state.lock().selected = Some(selected.clone());
            if !self.pool.unpoolable(&selected.key) && self.join(&selected).await {
                return;
            }
            let own = if self.pool.inner.legacy.lock().contains(&selected.key) { self.older.clone() } else { self.configured.clone() };
            self.state.lock().own = Some(own.clone());
            own.start().await;
        })
    }

    /// Lets go of the shared connection, or stops its own. The caller holds the Location's lock.
    pub fn stop(&self) -> BoxFuture<'_, ()> {
        Box::pin(async move {
            if self.state.lock().claim.is_none() {
                return self.private().stop().await;
            }
            self.clear().await;
            self.input.events.changed(Kind::Tools).await;
            self.input.events.changed(Kind::Resources).await;
            self.input.events.changed(Kind::Prompts).await;
        })
    }

    /// Requests through a shared connection bring this Location's elicitation handler along.
    pub async fn recovering<T, F, Fut>(&self, client: Arc<Connection>, run: F) -> Result<T, McpError>
    where
        F: Fn(Arc<Connection>, Option<Arc<dyn ElicitationHandler>>) -> Fut,
        Fut: std::future::Future<Output = Result<T, McpError>>,
    {
        let shared = self.state.lock().shared.clone();
        match shared {
            Some(shared) => {
                let elicit = self.elicit.clone();
                shared.recovering(client, |c| run(c, elicit.clone())).await
            }
            None => self.private().recovering(client, |c| run(c, None)).await,
        }
    }
}

/// A holder without an elicitation handler cancels every form.
struct Cancel;

#[async_trait]
impl ElicitationHandler for Cancel {
    async fn create(&self, _server: &str, _params: Value, _cancel: CancellationToken) -> Result<Value, String> {
        Ok(json!({ "action": "cancel" }))
    }

    async fn complete(&self, _server: &str, _elicitation_id: &str) {}
}

/// `pooled` in `makeEntry`: whether a Location shares this server. Local servers run on the Location's execution
/// plane, so only Locations on the host share them (ocd Locations always are).
pub fn pooled(config: &ServerConfig) -> bool {
    config.pool() && config.protocol() != Some(Protocol::Legacy)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn remote(value: Value) -> ServerConfig {
        serde_json::from_value(value).unwrap()
    }

    /// `keys connections on everything that changes them, and on no secret or name` (mcp.test.ts).
    #[test]
    fn keys_connections_on_everything_that_changes_them_and_on_no_secret_or_name() {
        let base = remote(json!({"type": "remote", "url": "https://mcp.example.com", "headers": {"authorization": "secret"}}));
        let local = remote(json!({"type": "local", "command": ["server"], "environment": {"TOKEN": "secret"}}));
        let key_of = |config: &ServerConfig, credential: Option<&str>| {
            key(&KeyInput { config, cwd: None, credential, client_info: None })
        };
        let local_key = |config: &ServerConfig, cwd: &str, client: Option<&ClientInfo>| {
            key(&KeyInput { config, cwd: Some(std::path::Path::new(cwd)), credential: None, client_info: client })
        };
        assert!(!key_of(&base, None).contains("secret"));
        assert!(!local_key(&local, "/a", None).contains("secret"));
        assert_ne!(key_of(&base, Some("cred_a")), key_of(&base, Some("cred_b")));
        let other = remote(json!({"type": "remote", "url": "https://mcp.example.com", "headers": {"authorization": "other"}}));
        assert_ne!(key_of(&other, None), key_of(&base, None));
        let raw = remote(json!({"type": "remote", "url": "https://mcp.example.com", "headers": {"authorization": "secret"}, "codemode": false}));
        assert_ne!(key_of(&raw, None), key_of(&base, None));
        let enabled = remote(json!({"type": "remote", "url": "https://mcp.example.com", "headers": {"authorization": "secret"}, "disabled": false}));
        assert_eq!(key_of(&enabled, None), key_of(&base, None));
        assert_ne!(local_key(&local, "/a", None), local_key(&local, "/b", None));
        let env = remote(json!({"type": "local", "command": ["server"], "environment": {"TOKEN": "other"}}));
        assert_ne!(local_key(&env, "/a", None), local_key(&local, "/a", None));
        let client = ClientInfo { name: "other".into(), version: "1".into() };
        assert_ne!(local_key(&local, "/a", Some(&client)), local_key(&local, "/a", None));
    }

    /// Every connection negotiates `auto` unless configured otherwise; only a `legacy` one is never pooled. The key
    /// reads the configured protocol as written.
    #[test]
    fn pools_every_protocol_but_legacy() {
        let unset = remote(json!({"type": "remote", "url": "https://x", "pool": true}));
        assert!(pooled(&unset));
        assert_eq!(unset.negotiation(), Protocol::Auto);
        let legacy = remote(json!({"type": "remote", "url": "https://x", "pool": true, "protocol": "legacy"}));
        assert!(!pooled(&legacy));
        let auto = remote(json!({"type": "remote", "url": "https://x", "pool": true, "protocol": "auto"}));
        let key_of = |config: &ServerConfig| key(&KeyInput { config, cwd: None, credential: None, client_info: None });
        assert_ne!(key_of(&auto), key_of(&unset));
        let plain = remote(json!({"type": "remote", "url": "https://x"}));
        assert!(!pooled(&plain));
    }
}
