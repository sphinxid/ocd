//! Web search backend (`core/src/websearch.ts`, `core/src/plugin/websearch/*`,
//! OpenCode Console provider from `core/src/plugin/provider/opencode.ts`).
//!
//! The service owns provider registration, the persisted/configured selection, the
//! per-session sticky "random" affinity and HTTP 429 cooldowns. Providers perform the
//! actual HTTP calls.

mod console;
mod exa;
mod firecrawl;
mod http;
mod parallel;
mod tavily;
mod tinyfish;

pub use console::{ConsoleCredential, ConsoleCredentialFn, ConsoleProvider, normalize_server, DEFAULT_SERVER};
pub use exa::ExaProvider;
pub use firecrawl::FirecrawlProvider;
pub use http::{MAX_RESPONSE_BYTES, parse_response};
pub use parallel::ParallelProvider;
pub use tavily::TavilyProvider;
pub use tinyfish::TinyFishProvider;

use async_trait::async_trait;
use parking_lot::{Mutex, RwLock};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::{HashMap, HashSet};
use std::sync::Arc;

/// KV key holding the persisted selection.
pub const PROVIDER_KEY: &str = "websearch:provider";
/// `App.make()` defaults: `opencode/${channel}/${version}/${name}`.
pub const DEFAULT_USER_AGENT: &str = "opencode/unknown/unknown/opencode";

/// `WebSearch.Selection` = provider id | `"random"` | `false`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Selection {
    Provider(String),
    Random,
    Disabled,
}

impl Selection {
    pub fn to_json(&self) -> Value {
        match self {
            Selection::Provider(id) => Value::String(id.clone()),
            Selection::Random => Value::String("random".into()),
            Selection::Disabled => Value::Bool(false),
        }
    }

    /// Decode like `Schema.Union([ID, Literal("random"), Literal(false)])`.
    pub fn from_json(value: &Value) -> Option<Selection> {
        match value {
            Value::String(s) if s == "random" => Some(Selection::Random),
            Value::String(s) => Some(Selection::Provider(s.clone())),
            Value::Bool(false) => Some(Selection::Disabled),
            _ => None,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProviderInfo {
    pub id: String,
    pub name: String,
}

/// `WebSearch.Result`: `{url, title?, content?, time:{published?}}` (published = ms since epoch).
#[derive(Clone, Debug, PartialEq, Default)]
pub struct SearchResult {
    pub url: String,
    pub title: Option<String>,
    pub content: Option<String>,
    pub published: Option<i64>,
}

#[derive(Serialize, Deserialize)]
struct SearchResultWire {
    url: String,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    title: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    content: Option<String>,
    time: TimeWire,
}

#[derive(Serialize, Deserialize)]
struct TimeWire {
    #[serde(skip_serializing_if = "Option::is_none", default)]
    published: Option<f64>,
}

impl Serialize for SearchResult {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        SearchResultWire {
            url: self.url.clone(),
            title: self.title.clone(),
            content: self.content.clone(),
            time: TimeWire { published: self.published.map(|p| p as f64) },
        }
        .serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for SearchResult {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let wire = SearchResultWire::deserialize(deserializer)?;
        if let Some(p) = wire.time.published
            && !p.is_finite()
        {
            return Err(serde::de::Error::custom("Expected a finite number"));
        }
        Ok(SearchResult {
            url: wire.url,
            title: wire.title,
            content: wire.content,
            published: wire.time.published.map(|p| p as i64),
        })
    }
}

/// `WebSearch.Response`: `{providerID, results}`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Response {
    #[serde(rename = "providerID")]
    pub provider_id: String,
    pub results: Vec<SearchResult>,
}

/// Failure of a provider call. `status` is set only for HTTP status failures
/// (Effect `HttpClientError` with a response), `retry_after` is that response's header.
#[derive(Clone, Debug, PartialEq)]
pub struct ProviderError {
    pub message: String,
    pub status: Option<u16>,
    pub retry_after: Option<String>,
}

impl ProviderError {
    pub fn new(message: impl Into<String>) -> Self {
        ProviderError { message: message.into(), status: None, retry_after: None }
    }

    pub fn status(status: u16, method: &str, url: &str, retry_after: Option<String>) -> Self {
        ProviderError {
            message: format!("StatusCode: non 2xx status code ({status} {method} {url})"),
            status: Some(status),
            retry_after,
        }
    }
}

impl std::fmt::Display for ProviderError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for ProviderError {}

#[async_trait]
pub trait SearchProvider: Send + Sync {
    fn info(&self) -> ProviderInfo;
    async fn execute(&self, query: &str) -> Result<Vec<SearchResult>, ProviderError>;
}

/// Persistence and credentials the service needs from core.
#[async_trait]
pub trait WebSearchStore: Send + Sync {
    async fn kv_get(&self, key: &str) -> Option<Value>;
    async fn kv_set(&self, key: &str, value: Value);
    async fn kv_remove(&self, key: &str);
    /// Active `key` credential for an integration (`connection.active` + `resolve`), if any.
    async fn api_key(&self, integration_id: &str) -> Option<String>;
}

#[derive(Clone, Debug, PartialEq)]
pub enum WebSearchError {
    /// `WebSearch.ProviderRequired`
    ProviderRequired,
    /// `WebSearch.ProviderNotFound`
    ProviderNotFound(String),
    /// `WebSearch.Disabled`
    Disabled,
    /// `WebSearch.Request`
    Request { provider_id: String, error: ProviderError },
}

impl WebSearchError {
    pub fn tag(&self) -> &'static str {
        match self {
            WebSearchError::ProviderRequired => "WebSearch.ProviderRequired",
            WebSearchError::ProviderNotFound(_) => "WebSearch.ProviderNotFound",
            WebSearchError::Disabled => "WebSearch.Disabled",
            WebSearchError::Request { .. } => "WebSearch.Request",
        }
    }
}

impl std::fmt::Display for WebSearchError {
    // Effect TaggedErrors without a message getter have an empty message.
    fn fmt(&self, _f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        Ok(())
    }
}

impl std::error::Error for WebSearchError {}

/// Integration auth method (`editor.method.update`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum IntegrationMethod {
    Key,
    Env { names: Vec<String> },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct IntegrationDescriptor {
    pub id: String,
    pub name: String,
    pub methods: Vec<IntegrationMethod>,
}

/// Integrations registered by the built-in web search plugins, in plugin order.
pub fn integrations() -> Vec<IntegrationDescriptor> {
    [
        ("exa", "Exa", "EXA_API_KEY"),
        ("firecrawl", "Firecrawl", "FIRECRAWL_API_KEY"),
        ("parallel", "Parallel", "PARALLEL_API_KEY"),
        ("tavily", "Tavily", "TAVILY_API_KEY"),
        ("tinyfish", "TinyFish", "TINYFISH_API_KEY"),
    ]
    .into_iter()
    .map(|(id, name, env)| IntegrationDescriptor {
        id: id.into(),
        name: name.into(),
        methods: vec![IntegrationMethod::Key, IntegrationMethod::Env { names: vec![env.into()] }],
    })
    .collect()
}

type Clock = Arc<dyn Fn() -> f64 + Send + Sync>;
type Affinity = Arc<Mutex<Option<String>>>;

#[derive(Clone)]
struct Cooldown {
    until: f64,
    error: WebSearchError,
}

struct Data {
    /// Insertion-ordered (JS `Map` semantics: re-adding keeps the original position).
    providers: Vec<Arc<dyn SearchProvider>>,
    selection: Option<Selection>,
}

impl Data {
    fn get(&self, id: &str) -> Option<Arc<dyn SearchProvider>> {
        self.providers.iter().find(|p| p.info().id == id).cloned()
    }
}

pub struct WebSearch {
    store: Arc<dyn WebSearchStore>,
    data: RwLock<Data>,
    cooldowns: Mutex<HashMap<String, Cooldown>>,
    preferred: Mutex<HashMap<Option<String>, Affinity>>,
    user_agent: Arc<RwLock<String>>,
    clock: RwLock<Clock>,
}

fn system_now() -> f64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as f64)
        .unwrap_or(0.0)
}

impl WebSearch {
    /// Service with the five built-in providers registered in upstream plugin order
    /// (exa, firecrawl, parallel, tavily, tinyfish).
    pub fn new(store: Arc<dyn WebSearchStore>, client: reqwest::Client) -> Self {
        let service = Self::empty(store.clone());
        let ua = service.user_agent.clone();
        service.add_provider(Arc::new(ExaProvider::new(client.clone(), store.clone())));
        service.add_provider(Arc::new(FirecrawlProvider::new(client.clone(), store.clone(), ua.clone())));
        service.add_provider(Arc::new(ParallelProvider::new(client.clone(), store.clone(), ua.clone())));
        service.add_provider(Arc::new(TavilyProvider::new(client.clone(), store.clone(), ua.clone())));
        service.add_provider(Arc::new(TinyFishProvider::new(client, store, ua)));
        service
    }

    /// Service without any providers.
    pub fn empty(store: Arc<dyn WebSearchStore>) -> Self {
        WebSearch {
            store,
            data: RwLock::new(Data { providers: Vec::new(), selection: None }),
            cooldowns: Mutex::new(HashMap::new()),
            preferred: Mutex::new(HashMap::new()),
            user_agent: Arc::new(RwLock::new(DEFAULT_USER_AGENT.to_string())),
            clock: RwLock::new(Arc::new(system_now)),
        }
    }

    /// `App.useragent(app)` sent by Firecrawl, Parallel, Tavily and TinyFish.
    pub fn set_user_agent(&self, user_agent: impl Into<String>) {
        *self.user_agent.write() = user_agent.into();
    }

    pub fn user_agent_handle(&self) -> Arc<RwLock<String>> {
        self.user_agent.clone()
    }

    /// Replace the millisecond clock (tests).
    pub fn set_clock(&self, clock: impl Fn() -> f64 + Send + Sync + 'static) {
        *self.clock.write() = Arc::new(clock);
    }

    fn now(&self) -> f64 {
        let clock = self.clock.read().clone();
        clock()
    }

    /// `editor.add`: a provider with an existing id replaces it in place.
    pub fn add_provider(&self, provider: Arc<dyn SearchProvider>) {
        let id = provider.info().id;
        let mut data = self.data.write();
        if let Some(slot) = data.providers.iter_mut().find(|p| p.info().id == id) {
            *slot = provider;
        } else {
            data.providers.push(provider);
        }
    }

    /// The registered providers in registration order.
    pub fn provider_list(&self) -> Vec<Arc<dyn SearchProvider>> {
        self.data.read().providers.clone()
    }

    /// Replace every provider (a rebuilt registry: built-ins → plugin transforms).
    pub fn set_providers(&self, providers: Vec<Arc<dyn SearchProvider>>) {
        self.data.write().providers = providers;
    }

    /// A new service over the same store, user agent and clock with this one's providers: one
    /// location's registry (upstream `WebSearch` is a location node; its own providers, selection,
    /// cooldowns and random-provider affinity).
    pub fn fork(&self) -> WebSearch {
        WebSearch {
            store: self.store.clone(),
            data: RwLock::new(Data { providers: self.provider_list(), selection: None }),
            cooldowns: Mutex::new(HashMap::new()),
            preferred: Mutex::new(HashMap::new()),
            user_agent: self.user_agent.clone(),
            clock: RwLock::new(self.clock.read().clone()),
        }
    }

    pub fn remove_provider(&self, id: &str) {
        self.data.write().providers.retain(|p| p.info().id != id);
    }

    /// `editor.default.set` from config (`websearch`) or the Console plugin. Overrides KV.
    pub fn set_config_selection(&self, selection: Option<Selection>) {
        self.data.write().selection = selection;
    }

    /// Providers sorted by display name (`localeCompare`).
    pub async fn providers(&self) -> Vec<ProviderInfo> {
        let mut list: Vec<ProviderInfo> = self.data.read().providers.iter().map(|p| p.info()).collect();
        list.sort_by(|a, b| locale_compare(&a.name, &b.name));
        list
    }

    async fn selection(&self) -> Option<Selection> {
        {
            let data = self.data.read();
            match &data.selection {
                Some(Selection::Disabled) => return Some(Selection::Disabled),
                Some(Selection::Random) => return Some(Selection::Random),
                Some(Selection::Provider(id)) if data.get(id).is_some() => {
                    return Some(Selection::Provider(id.clone()));
                }
                _ => {}
            }
        }
        let stored = self.store.kv_get(PROVIDER_KEY).await;
        let decoded = stored.as_ref().and_then(Selection::from_json);
        if stored.is_some() && decoded.is_none() {
            self.store.kv_remove(PROVIDER_KEY).await;
        }
        decoded
    }

    fn default_for(&self, choice: Option<&Selection>) -> Result<Option<Arc<dyn SearchProvider>>, WebSearchError> {
        match choice {
            Some(Selection::Disabled) => Err(WebSearchError::Disabled),
            Some(Selection::Random) => {
                // Inspection must not select a provider or reopen consent when every provider is cooling down.
                let active = self.preferred.lock().get(&None).and_then(|a| a.lock().clone());
                let data = self.data.read();
                Ok(active.and_then(|id| data.get(&id)).or_else(|| data.providers.first().cloned()))
            }
            Some(Selection::Provider(id)) => Ok(self.data.read().get(id)),
            None => Ok(None),
        }
    }

    /// `WebSearch.default()`.
    pub async fn default_provider(&self) -> Result<Option<ProviderInfo>, WebSearchError> {
        let choice = self.selection().await;
        Ok(self.default_for(choice.as_ref())?.map(|p| p.info()))
    }

    /// Persist the selection in KV.
    pub async fn select(&self, selection: Selection) {
        self.store.kv_set(PROVIDER_KEY, selection.to_json()).await;
    }

    /// Drop the session's random affinity (session deleted or moved).
    pub fn forget_session(&self, session_id: &str) {
        self.preferred.lock().remove(&Some(session_id.to_string()));
    }

    fn random_provider(&self, now: f64, affinity: &Affinity, attempted: Option<&HashSet<String>>) -> Option<Arc<dyn SearchProvider>> {
        let data = self.data.read();
        let mut cooldowns = self.cooldowns.lock();
        cooldowns.retain(|id, cooldown| !(cooldown.until <= now || data.get(id).is_none()));
        let tried = |id: &str| attempted.is_some_and(|set| set.contains(id));
        let mut cell = affinity.lock();
        if let Some(current) = cell.as_ref().and_then(|id| data.get(id)) {
            let id = current.info().id;
            if !cooldowns.contains_key(&id) && !tried(&id) {
                return Some(current);
            }
        }
        let available: Vec<_> = data
            .providers
            .iter()
            .filter(|p| {
                let id = p.info().id;
                !cooldowns.contains_key(&id) && !tried(&id)
            })
            .cloned()
            .collect();
        if available.is_empty() {
            return None;
        }
        let provider = available[rand::random_range(0..available.len())].clone();
        *cell = Some(provider.info().id);
        Some(provider)
    }

    /// `WebSearch.query`. `on_provider` is called before each provider attempt.
    pub async fn query(
        &self,
        query: &str,
        provider_id: Option<&str>,
        session_id: Option<&str>,
        on_provider: &(dyn Fn(&ProviderInfo) + Send + Sync),
    ) -> Result<Response, WebSearchError> {
        let choice = match provider_id {
            Some(_) => None,
            None => self.selection().await,
        };
        let provider = match provider_id {
            Some(id) => Some(self.data.read().get(id).ok_or_else(|| WebSearchError::ProviderNotFound(id.to_string()))?),
            None => self.default_for(choice.as_ref())?,
        };
        let Some(mut provider) = provider else {
            return Err(WebSearchError::ProviderRequired);
        };
        let random = choice == Some(Selection::Random);
        let key = session_id.map(str::to_string);
        // Keep the cell for this query so deletion/movement cannot reinsert an in-flight session's entry.
        let affinity: Affinity = self.preferred.lock().get(&key).cloned().unwrap_or_default();
        if random {
            self.preferred.lock().insert(key.clone(), affinity.clone());
            if let Some(p) = self.random_provider(self.now(), &affinity, None) {
                provider = p;
            }
        }
        let mut attempted = HashSet::new();
        loop {
            let info = provider.info();
            on_provider(&info);
            let mut cooldown = if random { self.cooldowns.lock().get(&info.id).cloned() } else { None };
            if cooldown.as_ref().is_none_or(|c| c.until <= self.now()) {
                attempted.insert(info.id.clone());
                match provider.execute(query).await {
                    Ok(results) => return Ok(Response { provider_id: info.id, results }),
                    Err(cause) => {
                        let status = cause.status;
                        let retry_after = cause.retry_after.clone();
                        let error = WebSearchError::Request { provider_id: info.id.clone(), error: cause };
                        if !random || status != Some(429) {
                            return Err(error);
                        }
                        let now = self.now();
                        let next = Cooldown { until: now + cooldown_millis(retry_after.as_deref(), now), error };
                        self.cooldowns.lock().insert(info.id.clone(), next.clone());
                        cooldown = Some(next);
                    }
                }
            }
            match self.random_provider(self.now(), &affinity, Some(&attempted)) {
                Some(next) => provider = next,
                None => {
                    return Err(cooldown.map(|c| c.error).unwrap_or(WebSearchError::ProviderRequired));
                }
            }
        }
    }
}

/// `cooldownMillis(retry-after, now)`.
pub fn cooldown_millis(value: Option<&str>, now: f64) -> f64 {
    let Some(value) = value.filter(|v| !v.trim().is_empty()) else {
        return 60_000.0;
    };
    if let Some(seconds) = js_number(value)
        && seconds.is_finite()
    {
        return if seconds >= 0.0 { seconds * 1000.0 } else { 60_000.0 };
    }
    match js_date_parse(value) {
        Some(date) => (date as f64 - now).max(0.0),
        None => 60_000.0,
    }
}

/// JavaScript `Number(string)` for non-empty input (NaN → None).
fn js_number(value: &str) -> Option<f64> {
    let trimmed = value.trim_matches(|c: char| c.is_whitespace() || c == '\u{feff}');
    if trimmed.is_empty() {
        return Some(0.0);
    }
    for (prefix, radix) in [("0x", 16), ("0X", 16), ("0o", 8), ("0O", 8), ("0b", 2), ("0B", 2)] {
        if let Some(digits) = trimmed.strip_prefix(prefix) {
            return u128::from_str_radix(digits, radix).ok().map(|v| v as f64);
        }
    }
    match trimmed {
        "Infinity" | "+Infinity" => return Some(f64::INFINITY),
        "-Infinity" => return Some(f64::NEG_INFINITY),
        _ => {}
    }
    let valid = trimmed.chars().all(|c| c.is_ascii_digit() || matches!(c, '.' | 'e' | 'E' | '+' | '-'));
    if !valid {
        return None;
    }
    trimmed.parse::<f64>().ok()
}

/// Approximation of JavaScript `Date.parse` for the formats providers emit:
/// ISO 8601 (date-only and offset forms as UTC, offset-less date-times as local time),
/// RFC 2822 / HTTP dates and a few long-form dates.
pub fn js_date_parse(value: &str) -> Option<i64> {
    use chrono::{DateTime, Local, NaiveDate, NaiveDateTime, TimeZone, Utc};
    let value = value.trim();
    if let Ok(dt) = DateTime::parse_from_rfc3339(value) {
        return Some(dt.timestamp_millis());
    }
    for format in ["%Y-%m-%dT%H:%M:%S%.f%:z", "%Y-%m-%dT%H:%M%:z", "%Y-%m-%dT%H:%M:%S%.fZ", "%Y-%m-%dT%H:%MZ"] {
        if let Ok(dt) = DateTime::parse_from_str(value, format) {
            return Some(dt.timestamp_millis());
        }
        if format.ends_with('Z')
            && let Ok(naive) = NaiveDateTime::parse_from_str(value, format)
        {
            return Some(Utc.from_utc_datetime(&naive).timestamp_millis());
        }
    }
    if let Ok(date) = NaiveDate::parse_from_str(value, "%Y-%m-%d") {
        return Some(Utc.from_utc_datetime(&date.and_hms_opt(0, 0, 0)?).timestamp_millis());
    }
    if value.len() == 7
        && let Ok(date) = NaiveDate::parse_from_str(&format!("{value}-01"), "%Y-%m-%d")
    {
        return Some(Utc.from_utc_datetime(&date.and_hms_opt(0, 0, 0)?).timestamp_millis());
    }
    if value.len() == 4
        && value.chars().all(|c| c.is_ascii_digit())
        && let Ok(date) = NaiveDate::parse_from_str(&format!("{value}-01-01"), "%Y-%m-%d")
    {
        return Some(Utc.from_utc_datetime(&date.and_hms_opt(0, 0, 0)?).timestamp_millis());
    }
    if let Ok(dt) = DateTime::parse_from_rfc2822(value) {
        return Some(dt.timestamp_millis());
    }
    for format in ["%Y-%m-%dT%H:%M:%S%.f", "%Y-%m-%dT%H:%M", "%Y-%m-%d %H:%M:%S", "%Y/%m/%d %H:%M:%S"] {
        if let Ok(naive) = NaiveDateTime::parse_from_str(value, format) {
            return Local.from_local_datetime(&naive).earliest().map(|d| d.timestamp_millis());
        }
    }
    for format in ["%B %d, %Y", "%b %d, %Y", "%d %B %Y", "%d %b %Y", "%Y/%m/%d"] {
        if let Ok(date) = NaiveDate::parse_from_str(value, format) {
            return Local.from_local_datetime(&date.and_hms_opt(0, 0, 0)?).earliest().map(|d| d.timestamp_millis());
        }
    }
    None
}

/// Rough ICU root-collation stand-in for `String.prototype.localeCompare`:
/// case-insensitive first, lowercase before uppercase on ties.
pub fn locale_compare(a: &str, b: &str) -> std::cmp::Ordering {
    let fold = |s: &str| s.to_lowercase();
    fold(a).cmp(&fold(b)).then_with(|| {
        let swap = |s: &str| {
            s.chars()
                .map(|c| if c.is_uppercase() { c.to_lowercase().next().unwrap_or(c) } else { c.to_uppercase().next().unwrap_or(c) })
                .collect::<String>()
        };
        swap(a).cmp(&swap(b))
    })
}

#[cfg(test)]
mod tests;
