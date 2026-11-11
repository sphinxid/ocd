//! The runtime half of the built-in `opencode.provider.github.copilot` plugin
//! (`packages/core/src/plugin/provider/github-copilot.ts`); the catalog transforms are in `oc_ai::catalog::copilot`.
//!
//! - **Loaded state** per location (`ProviderRefresh.start`): the active `github-copilot` OAuth connection's API
//!   base (the account's `/copilot_internal/user` endpoint, else the one stored at login), its token, and the
//!   account's live `/models` list, both through the process-wide [`SharedCache`]. Loaded when the location's
//!   catalog is first built (upstream: in the background when the plugin activates; here that first build waits
//!   for it), every 10 minutes, and on every `credential.switched` for the integration (the cached list at once,
//!   however old, then a forced load). Providers reload (`provider.updated`, `model.updated`) only when the
//!   loaded state changed.
//! - **`session.experimental.model.resolve`**: Copilot Auto. A session on the `auto` model is served by the model
//!   Copilot's router (`POST <base>/auto`) picks, kept until its token nears expiry, the session is compacted, or
//!   the account changes; when routing fails, the account's chat fallback.
//! - **`session.model.request`**: `X-Interaction-Type`, `X-Interaction-Id`, `x-initiator: agent` for background,
//!   compaction and subagent requests, and the Auto pick's `Copilot-Session-Token`.
//! - **`session.compaction`**: the request after a compaction routes Auto again.
//! - **`session.title`**: titles from the free utility model (`gpt-4o-mini`) when the account has it.
//! - **`session.http.request`**: the Copilot token as bearer and the Copilot headers on every request.

use std::collections::HashMap;
use std::sync::{Arc, Weak};
use std::time::Duration;

use indexmap::IndexMap;
use oc_ai::catalog::copilot::{decode_snapshot, Snapshot, State, API_VERSION, AUTO, PROVIDER_ID, UTILITY_TITLE_MODELS};
use oc_ai::credentials::flows::copilot::{normalize_domain, USER_API_VERSION};
use oc_ai::credentials::{CredentialValue, Integrations};
use oc_ai::route::transport::WireRequest;
use oc_ai::types::{ContentPart, Message, SystemPart};
use oc_schema::types::ModelRef;
use parking_lot::{Mutex, RwLock};
use serde_json::{json, Value};

use crate::bus::Bus;
use crate::plugin_request::Kind;
use crate::shared_cache::{LoadError, SharedCache, FOREVER, INTERVAL};

/// The built-in plugin id (gated by the location's `plugins` configuration).
pub const PLUGIN_ID: &str = "opencode.provider.github.copilot";

/// Test-only: replaces `https://api.github.com` (and an enterprise's `https://api.<domain>`) for the account
/// lookup, so tests can serve `/copilot_internal/user` locally.
pub const GITHUB_API_OVERRIDE: &str = "OCD_TEST_GITHUB_API";

/// How long before its token expires an Auto pick is routed again, and how long a fallback pick is kept.
const AUTO_MARGIN_MS: i64 = 5 * 60_000;

/// The model Copilot's Auto router picked for a session (`routes`).
#[derive(Debug, Clone, PartialEq)]
struct AutoRoute {
    model: String,
    /// The account token the pick was made with: a pick belongs to the account that made it.
    account: String,
    /// The session token that unlocks the pick and its Auto discount (none for a fallback).
    token: Option<String>,
    /// Epoch milliseconds until which the pick is kept.
    until: i64,
}

struct Location {
    key: String,
    state: RwLock<State>,
    /// `lock`: one `apply` at a time.
    applying: tokio::sync::Mutex<()>,
    /// `loading` (FiberHandle): the running refresh or switch.
    loading: Mutex<Option<tokio::task::JoinHandle<()>>>,
    /// The initial load has run.
    initialized: tokio::sync::OnceCell<()>,
    integrations: Arc<Integrations>,
    /// The `github-copilot` provider's headers from the latest catalog (`providers.get(...).headers`).
    headers: RwLock<Option<IndexMap<String, String>>>,
    /// Auto picks by session.
    routes: Mutex<HashMap<String, AutoRoute>>,
    /// The 10-minute refresh timer.
    timer: Mutex<Option<tokio::task::JoinHandle<()>>>,
}

impl Drop for Location {
    fn drop(&mut self) {
        if let Some(timer) = self.timer.get_mut().take() {
            timer.abort();
        }
        if let Some(loading) = self.loading.get_mut().take() {
            loading.abort();
        }
    }
}

/// The loaded locations running the plugin, with their integration registries (set by the server).
pub type LoadedLocations = Arc<dyn Fn() -> futures::future::BoxFuture<'static, Vec<(String, Arc<Integrations>)>> + Send + Sync>;

pub struct Copilot {
    locations: Mutex<HashMap<String, Arc<Location>>>,
    loaded: RwLock<Vec<LoadedLocations>>,
    http: reqwest::Client,
    user_agent: String,
    bus: Bus,
    cache: Arc<SharedCache>,
    /// [`GITHUB_API_OVERRIDE`] (or [`Copilot::set_github_api`]).
    github_api: RwLock<Option<String>>,
    subscribed: std::sync::Once,
    /// Bumped whenever a location's loaded state changes or a location is dropped: model catalogs
    /// built from an older state are stale.
    revision: std::sync::atomic::AtomicU64,
    me: Weak<Copilot>,
}

/// `App.useragent(ctx.app)`.
pub fn user_agent(version: &str) -> String {
    oc_ai::credentials::user_agent(oc_util::version::CHANNEL, version, oc_util::version::ARTIFACT)
}

/// What one load found: the connection it was made for and the state.
struct Loaded {
    connection: Option<String>,
    state: State,
}

impl Copilot {
    pub fn new(bus: Bus, version: &str) -> Arc<Copilot> {
        Arc::new_cyclic(|me| Copilot {
            locations: Mutex::new(HashMap::new()),
            loaded: RwLock::new(Vec::new()),
            http: oc_util::http::client(),
            user_agent: user_agent(version),
            bus,
            cache: crate::shared_cache::GLOBAL.clone(),
            github_api: RwLock::new(std::env::var(GITHUB_API_OVERRIDE).ok().filter(|v| !v.is_empty())),
            subscribed: std::sync::Once::new(),
            revision: std::sync::atomic::AtomicU64::new(0),
            me: me.clone(),
        })
    }

    /// Adds a source of the loaded locations, where `credential.switched` finds those whose catalog was not built
    /// yet (upstream's plugin runs in every location from its activation, so each one reloads).
    pub fn add_loaded_locations(&self, loaded: LoadedLocations) {
        self.loaded.write().push(loaded);
        if let Ok(runtime) = tokio::runtime::Handle::try_current() {
            self.subscribe(&runtime);
        }
    }

    fn location(&self, key: &str, integrations: &Arc<Integrations>) -> Arc<Location> {
        self.locations
            .lock()
            .entry(key.to_string())
            .or_insert_with(|| {
                Arc::new(Location {
                    key: key.to_string(),
                    state: RwLock::new(State::default()),
                    applying: tokio::sync::Mutex::new(()),
                    loading: Mutex::new(None),
                    initialized: tokio::sync::OnceCell::new(),
                    integrations: integrations.clone(),
                    headers: RwLock::new(None),
                    routes: Mutex::new(HashMap::new()),
                    timer: Mutex::new(None),
                })
            })
            .clone()
    }

    /// The loaded state of location `key` (empty before the first load).
    pub fn state(&self, key: &str) -> State {
        self.locations.lock().get(key).map(|l| l.state.read().clone()).unwrap_or_default()
    }

    /// Called after the first catalog build of location `key` (and harmless later): remembers the
    /// provider headers and runs the initial load (upstream's `refresh()` at activation) once, then refreshes
    /// every 10 minutes. Returns whether that load changed the state, i.e. the catalog must be rebuilt.
    pub async fn register(&self, key: &str, integrations: &Arc<Integrations>, headers: Option<IndexMap<String, String>>) -> bool {
        let location = self.location(key, integrations);
        *location.headers.write() = headers;
        if let Ok(runtime) = tokio::runtime::Handle::try_current() {
            self.subscribe(&runtime);
        }
        if location.initialized.initialized() {
            return false;
        }
        let mut changed = false;
        location
            .initialized
            .get_or_init(|| async {
                let loaded = self.load(&location, INTERVAL, false).await;
                changed = self.apply(&location, loaded).await;
            })
            .await;
        self.start_timer(&location);
        changed
    }

    /// Drops a closed location's state (the plugin scope ends with it).
    pub fn close(&self, key: &str) {
        if self.locations.lock().remove(key).is_some() {
            self.bump();
        }
    }

    /// Changes whenever any location's loaded state changes (part of the catalog cache key).
    pub fn revision(&self) -> u64 {
        self.revision.load(std::sync::atomic::Ordering::SeqCst)
    }

    fn bump(&self) {
        self.revision.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    }

    /// `apply`: takes a loaded value unless its connection is no longer the active one or it equals the current
    /// state. Returns whether the state changed (the caller reloads the providers).
    async fn apply(&self, location: &Location, loaded: Loaded) -> bool {
        let _permit = location.applying.lock().await;
        let active = location.integrations.active(PROVIDER_ID).await.ok().flatten().map(|c| c.key());
        if loaded.connection != active {
            return false;
        }
        let mut current = location.state.write();
        if *current == loaded.state {
            return false;
        }
        *current = loaded.state;
        drop(current);
        self.bump();
        true
    }

    /// `ctx.provider.reload()`: the Provider State notifies, then the Model State.
    fn reload(&self, location: &Location) {
        let at = Some(oc_schema::types::LocationRef { directory: location.key.clone(), workspace_id: None });
        self.bus.publish("provider.updated", json!({}), at.clone());
        self.bus.publish("model.updated", json!({}), at);
    }

    /// Loads and applies, reloading the providers when the state changed (`reloadIfChanged`).
    async fn refresh(self: &Arc<Self>, location: &Arc<Location>, interval: Duration, force: bool) {
        let loaded = self.load(location, interval, force).await;
        if self.apply(location, loaded).await {
            self.reload(location);
        }
    }

    /// Every 10 minutes, a refresh unless a load already runs (`FiberHandle.run(..., { onlyIfMissing: true })`).
    fn start_timer(&self, location: &Arc<Location>) {
        let mut timer = location.timer.lock();
        if timer.is_some() {
            return;
        }
        let Ok(runtime) = tokio::runtime::Handle::try_current() else { return };
        let me = self.me.clone();
        let weak = Arc::downgrade(location);
        *timer = Some(runtime.spawn(async move {
            loop {
                tokio::time::sleep(INTERVAL).await;
                let (Some(copilot), Some(location)) = (me.upgrade(), weak.upgrade()) else { return };
                let mut loading = location.loading.lock();
                if loading.as_ref().is_some_and(|task| !task.is_finished()) {
                    continue;
                }
                let target = location.clone();
                *loading = Some(tokio::spawn(async move { copilot.refresh(&target, INTERVAL, false).await }));
            }
        }));
    }

    /// `ProviderRefresh.switched`: after an account switch, applies the account's list from the cache at once,
    /// however old, while a forced load runs alongside and is applied when it finishes. With nothing cached the
    /// first step joins the forced load, so one request serves both. A switch replaces a waiting load.
    fn switched(self: &Arc<Self>, location: &Arc<Location>) {
        let copilot = self.clone();
        let target = location.clone();
        let task = tokio::spawn(async move {
            let forced = {
                let copilot = copilot.clone();
                let target = target.clone();
                tokio::spawn(async move { copilot.load(&target, INTERVAL, true).await })
            };
            copilot.refresh(&target, FOREVER, false).await;
            let _ = target.initialized.set(());
            if let Ok(fresh) = forced.await
                && copilot.apply(&target, fresh).await
            {
                copilot.reload(&target);
            }
        });
        if let Some(previous) = location.loading.lock().replace(task) {
            previous.abort();
        }
        self.start_timer(location);
    }

    /// `credential.switched` for `github-copilot` reloads every location: the ones whose catalog was built, and the
    /// other loaded ones running the plugin (whose load then counts as their initial one).
    fn subscribe(&self, runtime: &tokio::runtime::Handle) {
        let me = self.me.clone();
        let bus = self.bus.clone();
        self.subscribed.call_once(|| {
            let mut events = bus.subscribe();
            runtime.spawn(async move {
                loop {
                    let event = match events.recv().await {
                        Ok(event) => event,
                        Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                        Err(_) => return,
                    };
                    if event.kind != "credential.switched" || event.data.get("integrationID").and_then(Value::as_str) != Some(PROVIDER_ID) {
                        continue;
                    }
                    let Some(copilot) = me.upgrade() else { return };
                    let sources = copilot.loaded.read().clone();
                    for loaded in sources {
                        for (key, integrations) in loaded().await {
                            copilot.location(&key, &integrations);
                        }
                    }
                    let locations: Vec<Arc<Location>> = copilot.locations.lock().values().cloned().collect();
                    for location in locations {
                        copilot.switched(&location);
                    }
                }
            });
        });
    }

    /// `GithubCopilotPlugin.load`: the account's API endpoint (read again on every refresh, since it changes with
    /// the plan; the one stored at login is the fallback) and its model list, through the shared cache.
    async fn load(&self, location: &Location, interval: Duration, force: bool) -> Loaded {
        let integrations = &location.integrations;
        let connection = integrations.active(PROVIDER_ID).await.ok().flatten();
        let credential = match &connection {
            Some(c) => integrations.resolve(c).await.ok().flatten(),
            None => None,
        };
        let connection_key = connection.as_ref().map(|c| c.key());
        let (Some(key), Some(CredentialValue::Oauth { refresh, metadata, .. })) = (connection_key.clone(), credential) else {
            return Loaded { connection: connection_key, state: State::default() };
        };
        let enterprise = metadata.as_ref().and_then(|m| m.get("enterpriseUrl")).and_then(Value::as_str).map(String::from);
        let endpoint = {
            let url = self.user_url(enterprise.as_deref());
            let (http, user_agent, token) = (self.http.clone(), self.user_agent.clone(), refresh.clone());
            self.cache
                .get(&format!("github-copilot-endpoint:{key}"), interval, force, move || {
                    account_endpoint(http.clone(), url.clone(), token.clone(), user_agent.clone())
                })
                .await
                .ok()
                .flatten()
        };
        let url = endpoint.filter(|e| !e.is_empty()).unwrap_or_else(|| oc_ai::credentials::flows::copilot::copilot_base_url(metadata.as_ref()));
        let headers = location.headers.read().clone().unwrap_or_default();
        let models = {
            let models_key = models_key(&key, &url, &headers);
            let (http, user_agent, token, base) = (self.http.clone(), self.user_agent.clone(), refresh.clone(), url.clone());
            self.cache
                .get(&models_key, interval, force, move || fetch_models(http.clone(), base.clone(), headers.clone(), token.clone(), user_agent.clone()))
                .await
                .ok()
        };
        Loaded { connection: Some(key.clone()), state: State { base_url: Some(url), token: Some(refresh), models, connection: Some(key) } }
    }

    /// Test-only: the API base that replaces `https://api.github.com` for the account lookup.
    pub fn set_github_api(&self, base: Option<String>) {
        *self.github_api.write() = base;
    }

    /// `userURL(enterprise)`.
    fn user_url(&self, enterprise: Option<&str>) -> String {
        let api = match (self.github_api.read().clone(), enterprise) {
            (Some(api), _) => api.trim_end_matches('/').to_string(),
            (None, Some(enterprise)) => format!("https://api.{}", normalize_domain(enterprise)),
            (None, None) => "https://api.github.com".to_string(),
        };
        format!("{api}/copilot_internal/user")
    }

    /// The `experimental.model.resolve` hook: a session on Copilot's `auto` model is served by the model Copilot's
    /// router picked for it. Titles use Copilot's utility model, so they never route. `prompt` gives the session's
    /// last user message text and whether it carries an image. `None` leaves the model as it is.
    pub async fn resolve(&self, key: &str, session_id: &str, kind: Kind, model: &ModelRef, prompt: impl FnOnce() -> (String, bool)) -> Option<ModelRef> {
        if model.provider_id != PROVIDER_ID || model.id != AUTO || kind == Kind::Title {
            return None;
        }
        let location = self.locations.lock().get(key).cloned()?;
        let now = oc_schema::id::now_ms() as i64;
        let state = location.state.read().clone();
        let cached = location.routes.lock().get(session_id).cloned();
        let current = match cached {
            Some(route) if route.until > now && Some(&route.account) == state.token.as_ref() => Some(route),
            _ => self.route(&location, &state, prompt).await,
        }?;
        let now = oc_schema::id::now_ms() as i64;
        let mut routes = location.routes.lock();
        routes.retain(|_, route| route.until > now);
        routes.insert(session_id.to_string(), current.clone());
        Some(ModelRef { id: current.model, provider_id: PROVIDER_ID.into(), variant: None })
    }

    /// `route`: asks Copilot's router (`POST <base>/auto`, 5 s) for the session's model. Without an answer for a
    /// listed model, the account's chat fallback (or the first Auto pool model), routed again in 5 minutes.
    async fn route(&self, location: &Location, state: &State, prompt: impl FnOnce() -> (String, bool)) -> Option<AutoRoute> {
        let (Some(base_url), Some(token), Some(models)) = (&state.base_url, &state.token, &state.models) else { return None };
        let (text, image) = prompt();
        let mut body = serde_json::Map::new();
        body.insert("prompt".into(), json!(text));
        if image {
            body.insert("has_image".into(), json!(true));
        }
        let mut headers = location.headers.read().clone().unwrap_or_default();
        headers.insert("Authorization".into(), format!("Bearer {token}"));
        headers.insert("User-Agent".into(), self.user_agent.clone());
        headers.insert("X-GitHub-Api-Version".into(), API_VERSION.into());
        let mut request = self.http.post(format!("{base_url}/auto")).header("content-type", "application/json");
        for (name, value) in &headers {
            request = request.header(name.as_str(), value.as_str());
        }
        let request = request.headers(trace_headers()).body(Value::Object(body).to_string());
        let exchange = async {
            let response = request.send().await.map_err(|e| e.to_string())?;
            if !response.status().is_success() {
                return Err(format!("StatusCode: non 2xx status code ({})", response.status().as_u16()));
            }
            let value: Value = response.json().await.map_err(|e| e.to_string())?;
            decode_auto_route(&value).ok_or_else(|| "Expected { session_token, expires_at, selected_model: { id } }".to_string())
        };
        let picked = match tokio::time::timeout(Duration::from_secs(5), exchange).await {
            Ok(Ok(picked)) => Some(picked),
            Ok(Err(cause)) => {
                tracing::warn!(%cause, "Copilot Auto routing failed");
                None
            }
            Err(_) => {
                tracing::warn!(cause = "TimeoutError", "Copilot Auto routing failed");
                None
            }
        };
        let now = oc_schema::id::now_ms() as i64;
        if let Some((session_token, expires_at, selected)) = picked
            && models.contains_key(&selected)
        {
            // Route again before the token can expire mid-request.
            return Some(AutoRoute { model: selected, account: token.clone(), token: Some(session_token), until: (expires_at * 1000.0) as i64 - AUTO_MARGIN_MS });
        }
        // Retry routing soon when the router fails or picks a model this client cannot serve.
        let fallback = models
            .values()
            .find(|m| m.is_chat_fallback == Some(true))
            .or_else(|| models.values().find(|m| m.auto_discount.is_some_and(|d| d != 0.0 && !d.is_nan())))?;
        Some(AutoRoute { model: fallback.id.clone(), account: token.clone(), token: None, until: now + AUTO_MARGIN_MS })
    }

    /// The Auto pick's session token for a request on `model_id`: Copilot rejects it for any other model.
    pub fn session_token(&self, key: &str, session_id: &str, model_id: &str) -> Option<String> {
        let location = self.locations.lock().get(key).cloned()?;
        let routes = location.routes.lock();
        routes.get(session_id).filter(|route| route.model == model_id).and_then(|route| route.token.clone())
    }

    /// The `compaction` hook: compaction rewrites the prompt, so the request after it routes again, like Copilot's
    /// own clients. The compaction request itself still carries the current pick's token.
    pub fn compacted(&self, key: &str, session_id: &str) {
        let Some(location) = self.locations.lock().get(key).cloned() else { return };
        if let Some(route) = location.routes.lock().get_mut(session_id) {
            route.until = 0;
        }
    }
}

/// Effect's trace propagation headers (`b3`, `traceparent`), which the plugin's HTTP client sends like every
/// traced client.
fn trace_headers() -> reqwest::header::HeaderMap {
    let mut headers = reqwest::header::HeaderMap::new();
    for (name, value) in oc_util::trace::headers() {
        if let Ok(value) = reqwest::header::HeaderValue::from_str(&value) {
            headers.insert(name, value);
        }
    }
    headers
}

/// `AutoRoute`: `{session_token, expires_at, selected_model: {id}}`.
fn decode_auto_route(value: &Value) -> Option<(String, f64, String)> {
    let token = value.get("session_token")?.as_str()?.to_string();
    let expires = value.get("expires_at")?.as_f64()?;
    let selected = value.get("selected_model")?.as_object()?.get("id")?.as_str()?.to_string();
    Some((token, expires, selected))
}

/// `modelsKey`: the connection, base URL and a fingerprint of the provider headers.
fn models_key(connection: &str, url: &str, headers: &IndexMap<String, String>) -> String {
    // `JSON.stringify(Object.entries(headers).toSorted())`: entries sort by their `key,value` string.
    let mut entries: Vec<(&String, &String)> = headers.iter().collect();
    entries.sort_by(|a, b| {
        let key = |(k, v): &(&String, &String)| format!("{k},{v}").encode_utf16().collect::<Vec<u16>>();
        key(a).cmp(&key(b))
    });
    let json = serde_json::to_string(&entries.iter().map(|(k, v)| json!([k, v])).collect::<Vec<_>>()).unwrap_or_default();
    format!("github-copilot:{connection}:{url}:{}", crate::shared_cache::fingerprint(&json))
}

/// The account's Copilot API endpoint from `/copilot_internal/user` (5 s), without trailing slashes.
async fn account_endpoint(http: reqwest::Client, url: String, token: String, user_agent: String) -> Result<Option<String>, LoadError> {
    let exchange = async {
        let response = http
            .get(&url)
            .header("Accept", "application/json")
            .header("Authorization", format!("Bearer {token}"))
            .header("User-Agent", user_agent)
            .header("X-GitHub-Api-Version", USER_API_VERSION)
            .headers(trace_headers())
            .send()
            .await
            .map_err(|e| LoadError::request(&e))?;
        if !response.status().is_success() {
            return Err(LoadError::status(response.status().as_u16()));
        }
        let user: Value = response.json().await.map_err(|e| LoadError::new(e.to_string(), false))?;
        decode_user_endpoint(&user).ok_or_else(|| LoadError::new("Expected the Copilot user", false))
    };
    tokio::time::timeout(Duration::from_secs(5), exchange).await.map_err(|_| LoadError::new("TimeoutError", true))?
}

/// The `User` schema's `endpoints.api` (a mistyped field rejects the body), trailing slashes removed.
fn decode_user_endpoint(user: &Value) -> Option<Option<String>> {
    let user = user.as_object()?;
    if !user.get("chat_enabled").is_none_or(Value::is_boolean) || !user.get("can_signup_for_limited").is_none_or(Value::is_boolean) {
        return None;
    }
    let api = match user.get("endpoints") {
        None => None,
        Some(Value::Object(endpoints)) => match endpoints.get("api") {
            None => None,
            Some(Value::String(api)) => Some(api.trim_end_matches('/').to_string()),
            Some(_) => return None,
        },
        Some(_) => return None,
    };
    Some(api)
}

/// `CopilotModels.load`: `GET <base>/models` (5 s timeout).
async fn fetch_models(http: reqwest::Client, base_url: String, provider_headers: IndexMap<String, String>, token: String, user_agent: String) -> Result<Arc<Snapshot>, LoadError> {
    // `{...provider?.headers, Authorization, "User-Agent", "X-GitHub-Api-Version"}`.
    let mut headers = provider_headers;
    headers.insert("Authorization".into(), format!("Bearer {token}"));
    headers.insert("User-Agent".into(), user_agent);
    headers.insert("X-GitHub-Api-Version".into(), API_VERSION.into());
    let exchange = async {
        let mut request = http.get(format!("{base_url}/models"));
        for (name, value) in &headers {
            request = request.header(name.as_str(), value.as_str());
        }
        let response = request.headers(trace_headers()).send().await.map_err(|e| LoadError::request(&e))?;
        if !response.status().is_success() {
            return Err(LoadError::status(response.status().as_u16()));
        }
        let body: Value = response.json().await.map_err(|e| LoadError::new(e.to_string(), false))?;
        decode_snapshot(&body).map(Arc::new).map_err(|e| LoadError::new(e, false))
    };
    tokio::time::timeout(Duration::from_secs(5), exchange).await.map_err(|_| LoadError::new("TimeoutError", true))?
}

impl Copilot {
    /// The `session.title` hook: a title from the account's utility model, or `None` (the normal
    /// title request then runs). `agent_model`: the title agent configures its own model.
    pub async fn utility_title(
        &self,
        key: &str,
        session_id: &str,
        agent_model: bool,
        system: &[SystemPart],
        messages: &[Message],
        max_tokens: Option<u64>,
    ) -> Option<String> {
        let state = self.state(key);
        let (Some(base_url), Some(token), Some(models)) = (state.base_url, state.token, state.models) else { return None };
        if agent_model {
            return None;
        }
        let model = UTILITY_TITLE_MODELS.iter().find(|id| models.contains_key(**id))?;
        match self.send_utility_title(&base_url, &token, model, session_id, system, messages, max_tokens).await {
            Ok(title) => Some(title),
            Err(cause) => {
                tracing::debug!(model, %cause, "Copilot utility title failed");
                None
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    async fn send_utility_title(
        &self,
        base_url: &str,
        token: &str,
        model: &str,
        session_id: &str,
        system: &[SystemPart],
        messages: &[Message],
        max_tokens: Option<u64>,
    ) -> Result<String, String> {
        let text = |parts: &mut dyn Iterator<Item = Option<&str>>| parts.flatten().collect::<Vec<_>>().join("\n");
        let mut wire = vec![];
        if !system.is_empty() {
            let content = text(&mut system.iter().map(|p| (p.kind == "text").then_some(p.text.as_str())));
            wire.push(json!({"role": "system", "content": content}));
        }
        for message in messages {
            let content = text(&mut message.content.iter().map(|p| match p {
                ContentPart::Text(t) => Some(t.text.as_str()),
                _ => None,
            }));
            wire.push(json!({"role": message.role.as_str(), "content": content}));
        }
        let mut body = json!({"model": model, "messages": wire, "stream": false});
        if let Some(max) = max_tokens {
            body["max_tokens"] = json!(max);
        }
        let request = self
            .http
            .post(format!("{base_url}/chat/completions"))
            .header("Authorization", format!("Bearer {token}"))
            .header("Content-Type", "application/json")
            .header("User-Agent", self.user_agent.as_str())
            .header("X-GitHub-Api-Version", API_VERSION)
            .header("Openai-Intent", "conversation-edits")
            .header("X-Interaction-Type", "agent-session-name-generation")
            .header("X-Interaction-Id", session_id)
            .header("x-initiator", "agent")
            .body(serde_json::to_string(&body).map_err(|e| e.to_string())?);
        let exchange = async {
            let response = request.send().await.map_err(|e| e.to_string())?;
            if !response.status().is_success() {
                return Err(format!("Utility title request failed: {}", response.status().as_u16()));
            }
            response.text().await.map_err(|e| e.to_string())
        };
        let text = tokio::time::timeout(Duration::from_secs(20), exchange).await.map_err(|_| "timed out".to_string())??;
        let completion: Value = serde_json::from_str(&text).map_err(|e| e.to_string())?;
        let choices = completion.get("choices").and_then(Value::as_array).ok_or("invalid completion")?;
        // `UtilityCompletion`: every choice needs `message.content` (a string or null).
        for choice in choices {
            match choice.get("message").and_then(|m| m.get("content")) {
                Some(Value::String(_) | Value::Null) => {}
                _ => return Err("invalid completion".into()),
            }
        }
        let title = choices.first().and_then(|c| c["message"]["content"].as_str()).map(str::trim).unwrap_or_default();
        if title.is_empty() {
            return Err("Utility title response was empty".into());
        }
        Ok(title.to_string())
    }
}

/// `interactionType`: the Copilot client's `X-Interaction-Type` vocabulary.
pub fn interaction_type(kind: Kind, child: bool) -> &'static str {
    match kind {
        Kind::Title => "conversation-background",
        Kind::Compaction => "conversation-compaction",
        _ if child => "conversation-subagent",
        _ => "conversation-agent",
    }
}

/// The `session.model.request` hook on the request headers. The session's identity comes from the
/// headers the request was assembled with (`x-opencode-parent-session-id`, `x-session-affinity`).
/// `session_token`: the session's Auto pick token when the request is for the picked model.
pub fn model_request_headers(headers: &mut IndexMap<String, String>, kind: Kind, session_id: &str, session_token: Option<String>) {
    let child = headers.contains_key("x-opencode-parent-session-id");
    let affinity = headers.get("x-session-affinity").cloned().unwrap_or_else(|| session_id.to_string());
    let interaction = interaction_type(kind, child);
    headers.insert("X-Interaction-Type".into(), interaction.into());
    headers.insert("X-Interaction-Id".into(), affinity);
    if interaction != "conversation-agent" {
        headers.insert("x-initiator".into(), "agent".into());
    }
    if let Some(token) = session_token {
        headers.insert("Copilot-Session-Token".into(), token);
    }
}

/// `requestMetadata`: whether the request continues an agent turn and whether it carries images.
pub fn request_metadata(url: &str, body: Option<&Value>) -> (bool, bool) {
    let record = |v: &Value| v.is_object();
    let Some(body) = body.filter(|b| record(b)) else { return (false, false) };
    let typed = |part: &Value, kind: &str| part.is_object() && part.get("type").and_then(Value::as_str) == Some(kind);
    let user = |last: Option<&Value>| last.is_some_and(|l| record(l) && l.get("role").and_then(Value::as_str) == Some("user"));
    let content = |item: &Value| item.get("content").and_then(Value::as_array).cloned().unwrap_or_default();
    if let Some(input) = body.get("input").and_then(Value::as_array) {
        let agent = !user(input.last());
        // Tool results carry their images in `output`, user messages in `content`.
        let parts = |item: &Value, key: &str| item.get(key).and_then(Value::as_array).cloned().unwrap_or_default();
        let vision = input.iter().any(|item| record(item) && ["content", "output"].iter().any(|key| parts(item, key).iter().any(|p| typed(p, "input_image"))));
        return (agent, vision);
    }
    let Some(messages) = body.get("messages").and_then(Value::as_array) else { return (false, false) };
    let last = messages.last();
    if url.contains("completions") {
        // Chat Completions moves tool-result images into a user message after the tool messages.
        let previous = messages.len().checked_sub(2).and_then(|i| messages.get(i));
        let agent = !user(last) || previous.is_some_and(|p| record(p) && p.get("role").and_then(Value::as_str) == Some("tool"));
        let vision = messages.iter().any(|m| record(m) && content(m).iter().any(|p| typed(p, "image_url")));
        return (agent, vision);
    }
    let last_content = last.filter(|l| record(l)).map(content).unwrap_or_default();
    let agent = !user(last) || !last_content.iter().any(|p| p.is_object() && p.get("type").and_then(Value::as_str) != Some("tool_result"));
    let vision = messages.iter().any(|m| {
        record(m)
            && content(m).iter().any(|p| {
                typed(p, "image")
                    || (typed(p, "tool_result")
                        && p.get("content").and_then(Value::as_array).is_some_and(|nested| nested.iter().any(|n| typed(n, "image"))))
            })
    });
    (agent, vision)
}

/// The `session.http.request` hook: the Messages route sends the token as `x-api-key`, the OpenAI routes as a
/// bearer token; either becomes the bearer token, with the Copilot headers (`applyHeaders`).
pub fn http_request(request: &mut WireRequest, user_agent: &str) {
    let headers = &mut request.headers;
    let header = |name: &str| headers.iter().find(|(k, _)| k.eq_ignore_ascii_case(name)).map(|(_, v)| v.clone());
    let bearer = |value: String| {
        let lower = value.to_ascii_lowercase();
        match lower.strip_prefix("bearer") {
            Some(rest) if rest.starts_with(char::is_whitespace) => value[6..].trim_start().to_string(),
            _ => value,
        }
    };
    let Some(token) = header("x-api-key").or_else(|| header("authorization").map(bearer)).filter(|t| !t.is_empty()) else { return };
    let body = request.body.as_deref().and_then(|b| serde_json::from_str::<Value>(b).ok());
    let (agent, vision) = request_metadata(&request.url, body.as_ref());
    let set = |headers: &mut IndexMap<String, String>, name: &str, value: &str| {
        let existing = headers.keys().find(|k| k.eq_ignore_ascii_case(name)).cloned();
        match existing {
            Some(key) => {
                headers.insert(key, value.to_string());
            }
            None => {
                headers.insert(name.to_string(), value.to_string());
            }
        }
    };
    headers.retain(|k, _| !k.eq_ignore_ascii_case("authorization") && !k.eq_ignore_ascii_case("x-api-key"));
    set(headers, "authorization", &format!("Bearer {token}"));
    set(headers, "user-agent", user_agent);
    set(headers, "openai-intent", "conversation-edits");
    set(headers, "x-github-api-version", API_VERSION);
    // The step may already have declared itself agent-initiated; the body only escalates.
    if agent {
        set(headers, "x-initiator", "agent");
    } else if !headers.keys().any(|k| k.eq_ignore_ascii_case("x-initiator")) {
        set(headers, "x-initiator", "user");
    }
    if vision {
        set(headers, "copilot-vision-request", "true");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn interaction_headers() {
        let mut headers: IndexMap<String, String> =
            [("x-session-affinity".to_string(), "ses_root".to_string())].into_iter().collect();
        model_request_headers(&mut headers, Kind::Primary, "ses_x", None);
        assert_eq!(headers["X-Interaction-Type"], "conversation-agent");
        assert_eq!(headers["X-Interaction-Id"], "ses_root");
        assert!(!headers.contains_key("x-initiator"));
        let mut child: IndexMap<String, String> = [("x-opencode-parent-session-id".to_string(), "p".to_string())].into_iter().collect();
        model_request_headers(&mut child, Kind::Primary, "ses_child", Some("tok".into()));
        assert_eq!(child["Copilot-Session-Token"], "tok");
        assert_eq!(child["X-Interaction-Type"], "conversation-subagent");
        assert_eq!(child["X-Interaction-Id"], "ses_child");
        assert_eq!(child["x-initiator"], "agent");
        assert_eq!(interaction_type(Kind::Title, true), "conversation-background");
        assert_eq!(interaction_type(Kind::Compaction, false), "conversation-compaction");
    }

    #[test]
    fn metadata_by_wire_shape() {
        let responses = json!({"input": [{"role": "user", "content": [{"type": "input_image"}]}]});
        assert_eq!(request_metadata("http://x/responses", Some(&responses)), (false, true));
        let chat = json!({"messages": [{"role": "user", "content": "hi"}, {"role": "tool", "content": "x"}]});
        assert_eq!(request_metadata("http://x/chat/completions", Some(&chat)), (true, false));
        let messages = json!({"messages": [{"role": "user", "content": [{"type": "tool_result", "content": [{"type": "image"}]}]}]});
        assert_eq!(request_metadata("http://x/v1/messages", Some(&messages)), (true, true));
        let fresh = json!({"messages": [{"role": "user", "content": [{"type": "text", "text": "hi"}]}]});
        assert_eq!(request_metadata("http://x/v1/messages", Some(&fresh)), (false, false));
        assert_eq!(request_metadata("http://x", Some(&json!([1]))), (false, false));
    }

    #[test]
    fn native_route_headers() {
        let mut request = WireRequest {
            url: "http://x/v1/messages".into(),
            method: "POST".into(),
            headers: [("x-api-key".to_string(), "tok".to_string()), ("content-type".to_string(), "application/json".to_string())].into_iter().collect(),
            body: Some(json!({"messages": [{"role": "user", "content": [{"type": "text", "text": "hi"}]}]}).to_string()),
        };
        http_request(&mut request, "ua");
        assert_eq!(
            serde_json::to_value(&request.headers).unwrap(),
            json!({"content-type": "application/json", "authorization": "Bearer tok", "user-agent": "ua",
                   "openai-intent": "conversation-edits", "x-github-api-version": API_VERSION, "x-initiator": "user"})
        );
        // The OpenAI routes send the token as a bearer token.
        let mut chat = WireRequest {
            url: "http://x/chat/completions".into(),
            method: "POST".into(),
            headers: [("authorization".to_string(), "Bearer gho".to_string())].into_iter().collect(),
            body: Some(json!({"messages": [{"role": "user", "content": "hi"}, {"role": "tool", "content": "x"}, {"role": "user", "content": [{"type": "image_url"}]}]}).to_string()),
        };
        http_request(&mut chat, "ua");
        assert_eq!(chat.headers["authorization"], "Bearer gho");
        assert_eq!(chat.headers["x-initiator"], "agent");
        assert_eq!(chat.headers["copilot-vision-request"], "true");
        let mut untouched = WireRequest { url: "u".into(), method: "POST".into(), headers: IndexMap::new(), body: None };
        http_request(&mut untouched, "ua");
        assert!(untouched.headers.is_empty());
    }
}
