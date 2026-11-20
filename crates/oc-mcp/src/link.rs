//! One server connection, reconnected in place, with the catalogs it listed (`packages/core/src/mcp/connection.ts`,
//! `McpConnection`). A [`Link`] owns the [`Connection`] of one start; its owner (a Location's server entry, or the
//! pool's shared slot) hears about it through [`LinkEvents`], inline and in order.

use std::path::PathBuf;
use std::sync::{Arc, Weak};

use async_trait::async_trait;
use futures::future::BoxFuture;
use once_cell::sync::Lazy;
use parking_lot::Mutex;
use serde_json::{Map, Value};
use url::Url;

use crate::config::ServerConfig;
use crate::connection::{ConnectError, ConnectOptions, Connection};
use crate::error::McpError;
use crate::oauth::ConnectAuth;
use crate::protocol::ElicitationHandler;
use crate::types::{ClientInfo, Status};

/// A catalog the server can change: its tools, prompts or resources.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Tools,
    Prompts,
    Resources,
}

/// A raw listed item (tool or prompt), as the connection checked it.
pub type Listed = Arc<Vec<Map<String, Value>>>;

/// What a connection reports to its owner. Each runs inline, in order, so the owner can keep its own state and
/// publish in step with the connection.
#[async_trait]
pub trait LinkEvents: Send + Sync {
    /// An attempt is about to connect.
    async fn connecting(&self);
    /// The attempt connected and listed its tools.
    async fn connected(&self);
    /// `failure` is `failed` or `needs_auth`; `answered` is true when the server was reached but refused or broke
    /// the handshake.
    async fn failed(&self, failure: Status, answered: bool);
    /// The attempt ended, however it ended.
    async fn settled(&self);
    /// The live connection closed on its own. Runs under the lock; the owner decides how to stop.
    async fn closed(&self, reason: String);
    /// A catalog changed: the server said so, or the connection was torn down and every catalog emptied.
    async fn changed(&self, kind: Kind);
    /// Shared connections only: the server negotiated an older protocol than 2026-07-28, so the attempt closed right
    /// after the handshake, without listing tools. Reported instead of `connected` or `failed`.
    async fn legacy(&self, _version: Option<String>) {}
}

/// Runs work in the background: callbacks from the live connection, or recovery a background request triggered. A
/// runner may drop work, as a Location's does once it is closing.
pub trait Runner: Send + Sync {
    fn run(&self, work: BoxFuture<'static, ()>);
}

/// Spawns every piece of work on the current runtime.
pub struct Spawn;

impl Runner for Spawn {
    fn run(&self, work: BoxFuture<'static, ()>) {
        tokio::spawn(work);
    }
}

/// Builds the OAuth provider an attempt connects with. Read on every attempt, so a credential change applies to the
/// next one. `Err` fails the attempt with its text.
pub type AuthProvider = Arc<dyn Fn() -> BoxFuture<'static, Result<Option<Arc<ConnectAuth>>, String>> + Send + Sync>;

/// No OAuth provider.
pub fn no_auth() -> AuthProvider {
    Arc::new(|| Box::pin(async { Ok(None) }))
}

/// The per-server lifecycle lock (`lock`): callbacks and recovery take it; `start` and `stop` expect the caller to
/// hold it.
pub type Lock = Arc<tokio::sync::Mutex<()>>;

/// `McpConnection.Input`.
#[derive(Clone)]
pub struct LinkInput {
    pub name: String,
    pub config: ServerConfig,
    pub directory: PathBuf,
    /// Serves several Locations: it answers no `roots/list`, since no one directory owns it, and skips the
    /// per-endpoint handshake turn, since it is already the only connection for its key.
    pub shared: bool,
    pub client_info: ClientInfo,
    /// Declares MCP Events (`McpClient.ConnectOptions.events`).
    pub mcp_events: bool,
    pub auth_provider: AuthProvider,
    pub elicitation: Option<Arc<dyn ElicitationHandler>>,
    pub lock: Lock,
    /// Runs callbacks from the live connection.
    pub callbacks: Arc<dyn Runner>,
    /// Runs recovery that a background request triggered.
    pub fork: Arc<dyn Runner>,
    pub events: Arc<dyn LinkEvents>,
}

#[derive(Default)]
struct KeyedLocks {
    locks: Mutex<std::collections::HashMap<String, Lock>>,
}

impl KeyedLocks {
    fn get(&self, key: &str) -> Lock {
        self.locks.lock().entry(key.to_string()).or_default().clone()
    }
}

/// Unshared connections to one remote endpoint, from any number of Locations, take turns at their handshake so the
/// endpoint does not receive a startup burst (`endpointLoads`).
static ENDPOINT_LOADS: Lazy<KeyedLocks> = Lazy::new(KeyedLocks::default);

#[derive(Default)]
struct State {
    /// The attempt's scope: set from the start of an attempt until it fails or the connection stops.
    open: bool,
    client: Option<Arc<Connection>>,
    tools: Option<Listed>,
    prompts: Option<Listed>,
}

/// `McpConnection.make`: one server connection.
pub struct Link {
    input: LinkInput,
    state: Mutex<State>,
    this: Weak<Link>,
}

impl Link {
    pub fn new(input: LinkInput) -> Arc<Link> {
        Arc::new_cyclic(|this| Link { input, state: Mutex::new(State::default()), this: this.clone() })
    }

    pub fn input(&self) -> &LinkInput {
        &self.input
    }

    pub fn client(&self) -> Option<Arc<Connection>> {
        self.state.lock().client.clone()
    }

    /// The server's tool list as listed. Each refresh replaces it.
    pub fn tools(&self) -> Option<Listed> {
        self.state.lock().tools.clone()
    }

    pub fn prompts(&self) -> Option<Listed> {
        self.state.lock().prompts.clone()
    }

    fn is_live(&self, client: &Arc<Connection>) -> bool {
        self.state.lock().client.as_ref().is_some_and(|c| Arc::ptr_eq(c, client))
    }

    /// Runs a connection callback under the lock, dropping it if the connection is no longer the live client, so
    /// late callbacks cannot commit obsolete state.
    fn when_live<F>(&self, client: &Arc<Connection>, work: F)
    where
        F: FnOnce(Arc<Link>, Arc<Connection>) -> BoxFuture<'static, ()> + Send + 'static,
    {
        let Some(this) = self.this.upgrade() else { return };
        let client = client.clone();
        let lock = self.input.lock.clone();
        self.input.callbacks.run(Box::pin(async move {
            let _guard = lock.lock().await;
            if this.is_live(&client) {
                work(this.clone(), client).await;
            }
        }));
    }

    async fn refresh_tools(&self, client: &Arc<Connection>) -> Result<(), McpError> {
        let tools = client.tools().await?;
        self.state.lock().tools = Some(Arc::new(tools));
        Ok(())
    }

    /// A failed refresh keeps the previous prompts and publishes nothing.
    async fn refresh_prompts(&self, client: &Arc<Connection>) {
        let Ok(prompts) = client.prompts().await else { return };
        self.state.lock().prompts = Some(Arc::new(prompts));
        self.input.events.changed(Kind::Prompts).await;
    }

    /// Re-establishes a connection whose HTTP session the server dropped, unless another path already replaced it
    /// while this one waited for the lock.
    async fn recover(&self, client: &Arc<Connection>) {
        let _guard = self.input.lock.lock().await;
        if !self.is_live(client) {
            return;
        }
        tracing::info!(server = %self.input.name, "mcp session expired, reconnecting");
        self.stop().await;
        self.start().await;
    }

    /// Runs a request against `client` and, if that request observed a session expiry, reconnects and runs it once
    /// more against the replacement. Any other failure passes through.
    pub async fn recovering<T, F, Fut>(&self, client: Arc<Connection>, run: F) -> Result<T, McpError>
    where
        F: Fn(Arc<Connection>) -> Fut,
        Fut: std::future::Future<Output = Result<T, McpError>>,
    {
        match run(client.clone()).await {
            Err(error @ McpError::SessionExpired { .. }) => {
                self.recover(&client).await;
                match self.client() {
                    Some(next) => run(next).await,
                    None => Err(error),
                }
            }
            other => other,
        }
    }

    fn watch(&self, client: &Arc<Connection>) {
        let weak = self.this.clone();
        let target = Arc::downgrade(client);
        client.on_close(move |reason| {
            let (Some(this), Some(client)) = (weak.upgrade(), target.upgrade()) else { return };
            this.when_live(&client, move |this, _| Box::pin(async move { this.input.events.closed(reason).await }));
        });
        // Background refreshes (list-changed) can observe the expiry too; they do not retry, so reconnect here for
        // them. Foreground calls reconnect through `recovering`.
        let weak = self.this.clone();
        let target = Arc::downgrade(client);
        client.on_session_expired(move || {
            let (Some(this), Some(client)) = (weak.upgrade(), target.upgrade()) else { return };
            let fork = this.input.fork.clone();
            fork.run(Box::pin(async move { this.recover(&client).await }));
        });
        let weak = self.this.clone();
        let target = Arc::downgrade(client);
        client.on_tools_changed(move || {
            let (Some(this), Some(client)) = (weak.upgrade(), target.upgrade()) else { return };
            this.when_live(&client, |this, client| {
                Box::pin(async move {
                    if this.refresh_tools(&client).await.is_ok() {
                        this.input.events.changed(Kind::Tools).await;
                    }
                })
            });
        });
        let weak = self.this.clone();
        let target = Arc::downgrade(client);
        client.on_prompts_changed(move || {
            let (Some(this), Some(client)) = (weak.upgrade(), target.upgrade()) else { return };
            this.when_live(&client, |this, client| Box::pin(async move { this.refresh_prompts(&client).await }));
        });
        let weak = self.this.clone();
        let target = Arc::downgrade(client);
        client.on_resources_changed(move || {
            let (Some(this), Some(client)) = (weak.upgrade(), target.upgrade()) else { return };
            this.when_live(&client, |this, _| Box::pin(async move { this.input.events.changed(Kind::Resources).await }));
        });
    }

    /// Connects and lists tools; a failure is reported through `failed`. The caller holds the lock.
    pub fn start(&self) -> BoxFuture<'_, ()> {
        Box::pin(async move {
            let input = &self.input;
            input.events.connecting().await;
            self.attempt().await;
            input.events.settled().await;
        })
    }

    async fn attempt(&self) {
        let input = &self.input;
        // Checked before the OAuth provider is built, which parses the URL too.
        if let ServerConfig::Remote(remote) = &input.config
            && Url::parse(&remote.url).is_err()
        {
            return input.events.failed(Status::failed(format!("Invalid MCP server URL: {}", remote.url)), false).await;
        }
        self.state.lock().open = true;
        let result = async {
            let auth_provider = (input.auth_provider)().await.map_err(|message| ConnectError::connect(&input.name, message))?;
            // List tools as part of connect so a failure here marks the server failed rather than leaving it
            // connected with a silently empty tool list. A shared connection decides first: it can only be shared on
            // 2026-07-28, so an older server is never asked to list.
            let _endpoint = match &input.config {
                ServerConfig::Remote(remote) if !input.shared => Some(ENDPOINT_LOADS.get(&remote.url).lock_owned().await),
                _ => None,
            };
            let client = Connection::open(ConnectOptions {
                server: &input.name,
                config: &input.config,
                directory: &input.directory,
                auth_provider,
                elicitation: input.elicitation.clone(),
                client_info: &input.client_info,
                roots: !input.shared,
                events: input.mcp_events,
            })
            .await?;
            if input.shared && !client.modern() {
                return Ok((client, None));
            }
            match client.tools().await {
                Ok(tools) => Ok((client, Some(tools))),
                Err(error) => {
                    client.close().await;
                    Err(ConnectError::connect(&input.name, error.to_string()))
                }
            }
        }
        .await;
        match result {
            Ok((client, None)) => {
                client.close().await;
                self.state.lock().open = false;
                input.events.legacy(client.protocol_version()).await;
            }
            Ok((client, Some(tools))) => {
                let client = Arc::new(client);
                {
                    let mut state = self.state.lock();
                    state.client = Some(client.clone());
                    state.tools = Some(Arc::new(tools));
                    state.prompts = Some(Arc::new(Vec::new()));
                }
                self.watch(&client);
                input.events.connected().await;
                self.when_live(&client, |this, client| Box::pin(async move { this.refresh_prompts(&client).await }));
            }
            Err(error) => {
                self.state.lock().open = false;
                let (status, answered) = match error {
                    ConnectError::NeedsAuth { message, .. } => (Status::NeedsAuth { error: message }, false),
                    ConnectError::Connect { message, answered, .. } => (Status::Failed { error: message }, answered),
                };
                input.events.failed(status, answered).await;
            }
        }
    }

    /// Closes the connection and empties every catalog. The caller holds the lock.
    pub fn stop(&self) -> BoxFuture<'_, ()> {
        Box::pin(async move {
            let client = {
                let mut state = self.state.lock();
                if !state.open {
                    return;
                }
                state.open = false;
                state.tools = None;
                state.prompts = None;
                state.client.take()
            };
            if let Some(client) = client {
                client.close().await;
            }
            self.input.events.changed(Kind::Tools).await;
            self.input.events.changed(Kind::Resources).await;
            self.input.events.changed(Kind::Prompts).await;
        })
    }
}
