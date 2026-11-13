//! The local model discovery plugins `opencode.provider.lmstudio`, `opencode.provider.ollama` and
//! `opencode.provider.vllm` (`packages/core/src/plugin/provider/{local,lmstudio,ollama,vllm}.ts`); their provider
//! transform is `oc_ai::catalog::local`.
//!
//! Each plugin runs per location from the location's boot: it asks its server for the models it serves right away,
//! then every 30 seconds, and again when the location's configuration changes the provider's `baseURL` or `apiKey`
//! (`config.updated`). Without a configured `baseURL` it asks the default origin (LM Studio
//! `http://127.0.0.1:1234`, Ollama `http://127.0.0.1:11434`, vLLM `http://127.0.0.1:8000`). Every request has a
//! 1 second limit and carries `Authorization: Bearer <apiKey>` when one is configured.
//!
//! - LM Studio: `GET <root>/api/v1/models`, the `llm` models (`/v1` and `/api/v1` suffixes of the base URL are
//!   stripped to find the root).
//! - Ollama: `GET <root>/api/tags`, then `POST <root>/api/show` for each model (4 at a time, cached by digest), the
//!   ones with the `completion` capability (`/v1` and `/api` suffixes stripped).
//! - vLLM: `GET <root>/health`, then `GET <root>/v1/models`, the models `owned_by` `vllm`.
//!
//! An answer that changed the inventory reloads the integrations and providers (`integration.updated` when the
//! integration list changed, `provider.updated`, `model.updated`). A failed request keeps the last inventory; an
//! empty answer restores the models.dev definition. Answers are cached per server root and API key for the
//! refresh interval across locations (one request at a time per plugin).
//!
//! Upstream's plugins start inside the boot's `State.batch`, so an inventory a server answers at once is part of the
//! boot's notifications: the server's boot waits up to 250 ms for the first round ([`LocalProviders::first_round`]),
//! and what comes before [`LocalProviders::booted`] announces nothing of its own. A configuration reload is followed
//! after the reloaded location announced itself ([`LocalProviders::reconfigure`]), and each reload's notifications
//! are published before the plugin goes on, so the integration list compares with the state at that point.
//!
//! Discovery runs only in a server ([`LocalProviders::enable`]); [`ORIGIN_OVERRIDE`] replaces the default origins
//! for tests.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Weak};
use std::time::{Duration, Instant};

use futures::StreamExt;
use oc_ai::catalog::local::{configured, fold_settings, Discovered, Kind, LocalModel, Source};
use oc_config::schema::Entry;
use parking_lot::{Mutex, RwLock};
use serde_json::{Map, Value};

use crate::bus::Bus;

/// Test-only: replaces the default origins, so tests never reach a real server on 1234, 11434 or 8000. Either one
/// origin for all three plugins (`http://127.0.0.1:9`), `off` (no default discovery), or per provider
/// (`lmstudio=http://127.0.0.1:9,ollama=off`; providers left out keep their default).
pub const ORIGIN_OVERRIDE: &str = "OCD_TEST_LOCAL_PROVIDER_ORIGIN";

/// How often each plugin asks again.
pub const INTERVAL: Duration = Duration::from_secs(30);

/// The limit of every discovery request.
const TIMEOUT: Duration = Duration::from_secs(1);

/// The default origins with [`ORIGIN_OVERRIDE`] applied (`None`: no default discovery).
fn default_origins(value: Option<&str>) -> [Option<String>; 3] {
    let mut out = Kind::ALL.map(|k| Some(k.origin().to_string()));
    let Some(value) = value.map(str::trim).filter(|v| !v.is_empty()) else { return out };
    let parse = |v: &str| (v != "off").then(|| v.to_string());
    if !value.contains('=') {
        return Kind::ALL.map(|_| parse(value));
    }
    for part in value.split(',') {
        let Some((id, origin)) = part.split_once('=') else { continue };
        if let Some(kind) = Kind::from_provider_id(id.trim()) {
            out[index(kind)] = parse(origin.trim());
        }
    }
    out
}

fn index(kind: Kind) -> usize {
    Kind::ALL.iter().position(|k| *k == kind).unwrap_or(0)
}

// ------------------------------------------------------------------------------------------------ response schemas

/// The subset of Effect Schema the plugins decode their answers with: excess keys are dropped, optional keys may be
/// absent but not `null`, integers are safe integers.
#[derive(Debug, Clone)]
enum Schema {
    Str,
    Int,
    Bool,
    Literals(&'static [&'static str]),
    NullOr(Box<Schema>),
    Array(Box<Schema>),
    /// `(key, schema, optional)`.
    Struct(Vec<(&'static str, Schema, bool)>),
    /// `Record(String, Unknown)`.
    Record,
}

fn decode(schema: &Schema, value: &Value) -> Result<Value, String> {
    let fail = |expected: &str| Err(format!("Expected {expected}, got {value}"));
    match schema {
        Schema::Str => value.as_str().map(|_| value.clone()).map_or_else(|| fail("string"), Ok),
        Schema::Bool => value.as_bool().map(|_| value.clone()).map_or_else(|| fail("boolean"), Ok),
        Schema::Int => match value.as_f64() {
            Some(n) if n.fract() == 0.0 && n.abs() <= 9_007_199_254_740_991.0 => Ok(value.clone()),
            _ => fail("an integer"),
        },
        Schema::Literals(options) => match value.as_str() {
            Some(s) if options.contains(&s) => Ok(value.clone()),
            _ => fail(&format!("{options:?}")),
        },
        Schema::NullOr(inner) => if value.is_null() { Ok(Value::Null) } else { decode(inner, value) },
        Schema::Array(inner) => match value.as_array() {
            Some(items) => items.iter().map(|item| decode(inner, item)).collect::<Result<Vec<_>, _>>().map(Value::Array),
            None => fail("array"),
        },
        Schema::Struct(fields) => {
            let Some(object) = value.as_object() else { return fail("object") };
            let mut out = Map::new();
            for (key, field, optional) in fields {
                match object.get(*key) {
                    None if *optional => {}
                    None => return Err(format!("Missing key {key}")),
                    Some(v) => {
                        out.insert(key.to_string(), decode(field, v).map_err(|e| format!("{key}: {e}"))?);
                    }
                }
            }
            Ok(Value::Object(out))
        }
        Schema::Record => value.as_object().map(|_| value.clone()).map_or_else(|| fail("object"), Ok),
    }
}

fn strings() -> Schema {
    Schema::Array(Box::new(Schema::Str))
}

fn lmstudio_response() -> Schema {
    use Schema::*;
    Struct(vec![(
        "models",
        Array(Box::new(Struct(vec![
            ("type", Literals(&["llm", "embedding"]), false),
            ("key", Str, false),
            ("display_name", Str, false),
            ("architecture", NullOr(Box::new(Str)), true),
            ("loaded_instances", Array(Box::new(Struct(vec![("config", Struct(vec![("context_length", Int, false)]), false)]))), false),
            ("max_context_length", Int, false),
            ("capabilities", Struct(vec![("vision", Bool, false), ("trained_for_tool_use", Bool, false)]), true),
        ]))),
        false,
    )])
}

fn ollama_details() -> Schema {
    use Schema::*;
    Struct(vec![
        ("parent_model", Str, true),
        ("format", Str, false),
        ("family", Str, false),
        ("families", strings(), true),
        ("parameter_size", Str, false),
        ("quantization_level", Str, false),
    ])
}

fn ollama_tags() -> Schema {
    use Schema::*;
    Struct(vec![(
        "models",
        Array(Box::new(Struct(vec![
            ("name", Str, false),
            ("model", Str, false),
            ("remote_model", Str, true),
            ("remote_host", Str, true),
            ("modified_at", Str, false),
            ("size", Int, false),
            ("digest", Str, false),
            ("details", ollama_details(), false),
        ]))),
        false,
    )])
}

fn ollama_show() -> Schema {
    use Schema::*;
    Struct(vec![
        ("parameters", Str, true),
        ("license", Str, true),
        ("modified_at", Str, true),
        ("details", ollama_details(), true),
        ("template", Str, true),
        ("capabilities", strings(), true),
        ("model_info", Record, true),
    ])
}

fn vllm_response() -> Schema {
    use Schema::*;
    Struct(vec![(
        "data",
        Array(Box::new(Struct(vec![("id", Str, false), ("owned_by", Str, false), ("max_model_len", NullOr(Box::new(Int)), false)]))),
        false,
    )])
}

fn str_of<'a>(value: &'a Value, key: &str) -> &'a str {
    value.get(key).and_then(Value::as_str).unwrap_or_default()
}

fn has(list: Option<&Value>, item: &str) -> Option<bool> {
    list.and_then(Value::as_array).map(|l| l.iter().any(|v| v.as_str() == Some(item)))
}

/// `model(item)` of each plugin.
fn model(kind: Kind, item: &Value) -> LocalModel {
    match kind {
        Kind::LmStudio => {
            let instances = item["loaded_instances"].as_array().cloned().unwrap_or_default();
            let context = if instances.is_empty() {
                item["max_context_length"].as_f64()
            } else {
                instances.iter().filter_map(|i| i["config"]["context_length"].as_f64()).reduce(f64::min)
            };
            let key = str_of(item, "key");
            let display = str_of(item, "display_name");
            LocalModel {
                id: key.to_string(),
                name: Some(if display.is_empty() { key } else { display }.to_string()),
                family: item.get("architecture").and_then(Value::as_str).map(String::from),
                tools: item.get("capabilities").and_then(|c| c["trained_for_tool_use"].as_bool()),
                vision: item.get("capabilities").and_then(|c| c["vision"].as_bool()),
                context,
            }
        }
        Kind::Ollama => {
            let id = str_of(item, "model");
            let name = str_of(item, "name");
            let show = &item["show"];
            let show_family = show.get("details").map(|d| str_of(d, "family")).unwrap_or_default();
            let family = if show_family.is_empty() { str_of(&item["details"], "family") } else { show_family };
            let context = show
                .get("model_info")
                .and_then(Value::as_object)
                .and_then(|info| info.iter().find_map(|(k, v)| (k.ends_with(".context_length") && v.as_f64().is_some_and(|n| n > 0.0)).then(|| v.as_f64()).flatten()));
            LocalModel {
                id: id.to_string(),
                name: Some(if name.is_empty() { id } else { name }.to_string()),
                family: Some(family.to_string()),
                tools: has(show.get("capabilities"), "tools"),
                vision: has(show.get("capabilities"), "vision"),
                context,
            }
        }
        Kind::Vllm => LocalModel { id: str_of(item, "id").to_string(), context: item["max_model_len"].as_f64(), ..Default::default() },
    }
}

fn sort_by_key(items: &mut [Value], key: &str) {
    items.sort_by(|a, b| oc_schema::collate::locale_compare(str_of(a, key), str_of(b, key)));
}

// ------------------------------------------------------------------------------------------------ discovery client

/// `createClient(http, root, apiKey, endpoint)`.
struct Client<'a> {
    http: &'a reqwest::Client,
    source: &'a Source,
}

impl Client<'_> {
    fn request(&self, method: reqwest::Method, path: &str) -> Result<reqwest::RequestBuilder, String> {
        let url = self.source.endpoint(path).ok_or_else(|| "no endpoint".to_string())?;
        let mut request = self.http.request(method, url).header("accept", "application/json");
        if let Some(key) = self.source.api_key.as_deref().filter(|k| !k.is_empty()) {
            request = request.header("authorization", format!("Bearer {key}"));
        }
        for (name, value) in oc_util::trace::headers() {
            request = request.header(name, value);
        }
        Ok(request)
    }

    async fn send(&self, request: reqwest::RequestBuilder) -> Result<bytes::Bytes, String> {
        let exchange = async {
            let response = request.send().await.map_err(|e| e.to_string())?;
            if !response.status().is_success() {
                return Err(format!("StatusCode: non 2xx status code ({})", response.status().as_u16()));
            }
            response.bytes().await.map_err(|e| e.to_string())
        };
        tokio::time::timeout(TIMEOUT, exchange).await.map_err(|_| "TimeoutError".to_string())?
    }

    async fn get(&self, path: &str) -> Result<(), String> {
        self.send(self.request(reqwest::Method::GET, path)?).await.map(|_| ())
    }

    async fn get_json(&self, path: &str, schema: &Schema) -> Result<Value, String> {
        let body = self.send(self.request(reqwest::Method::GET, path)?).await?;
        decode(schema, &serde_json::from_slice(&body).map_err(|e| e.to_string())?)
    }

    async fn post_json(&self, path: &str, body: Value, schema: &Schema) -> Result<Value, String> {
        let request = self.request(reqwest::Method::POST, path)?.header("content-type", "application/json").body(body.to_string());
        let body = self.send(request).await?;
        decode(schema, &serde_json::from_slice(&body).map_err(|e| e.to_string())?)
    }
}

/// A server root's last answer (`discovery` in local.ts).
struct Cached {
    checked: Instant,
    api_key: Option<String>,
    models: Option<Vec<Value>>,
}

/// Ollama's `/api/show` answers per root, kept while the model's digest is unchanged.
struct Shows {
    api_key: Option<String>,
    entries: HashMap<String, (String, Value)>,
}

/// What one plugin shares across its locations (module state in upstream).
#[derive(Default)]
struct Shared {
    lock: tokio::sync::Mutex<()>,
    discovery: Mutex<HashMap<String, Cached>>,
    shows: Mutex<HashMap<String, Shows>>,
}

// ------------------------------------------------------------------------------------------------ locations

/// One plugin at one location.
struct Slot {
    source: RwLock<Arc<Source>>,
    /// The last inventory: the decoded items (compared to detect a change) and their models.
    loaded: RwLock<(Vec<Value>, Vec<LocalModel>)>,
}

struct Location {
    key: String,
    entries: RwLock<Vec<Entry>>,
    slots: [Slot; 3],
    /// One plugin applies a change at a time (its reload compares with what the location announced before).
    applying: tokio::sync::Mutex<()>,
    started: AtomicBool,
    /// The location's boot has not announced itself yet: an inventory found meanwhile is part of the boot's
    /// notifications (upstream's plugins activate inside the boot's `State.batch`).
    booting: AtomicBool,
    /// How many plugins finished their first round.
    first_round: tokio::sync::watch::Sender<usize>,
    tasks: Mutex<Vec<tokio::task::JoinHandle<()>>>,
}

impl Drop for Location {
    fn drop(&mut self) {
        for task in self.tasks.get_mut().drain(..) {
            task.abort();
        }
    }
}

/// The three plugins across locations.
pub struct LocalProviders {
    bus: Bus,
    plugins: Weak<crate::plugins::Plugins>,
    http: reqwest::Client,
    enabled: AtomicBool,
    /// The server reloads a location's services after `config.updated` and calls [`LocalProviders::reconfigure`]
    /// once that reload announced itself; otherwise the plugins follow `config.updated` themselves.
    follow_rebuilds: AtomicBool,
    interval: RwLock<Duration>,
    origins: RwLock<[Option<String>; 3]>,
    shared: [Shared; 3],
    locations: Mutex<HashMap<String, Arc<Location>>>,
    subscribed: std::sync::Once,
    me: Weak<LocalProviders>,
}

/// The provider settings each config document sets for `provider_id` (`providers[id].settings`).
fn settings_of(entries: &[Entry], provider_id: &str) -> Option<Map<String, Value>> {
    let documents: Vec<Option<Map<String, Value>>> = entries
        .iter()
        .map(|entry| match entry {
            Entry::Document { info, .. } => info
                .providers
                .as_ref()
                .and_then(|p| serde_json::to_value(p).ok())
                .and_then(|p| p.get(provider_id).and_then(|p| p.get("settings")).and_then(Value::as_object).cloned()),
            Entry::Directory { .. } => None,
        })
        .collect();
    fold_settings(documents.iter().map(Option::as_ref))
}

impl LocalProviders {
    pub fn new(bus: Bus, plugins: Weak<crate::plugins::Plugins>) -> Arc<LocalProviders> {
        Arc::new_cyclic(|me| LocalProviders {
            bus,
            plugins,
            http: oc_util::http::client(),
            enabled: AtomicBool::new(false),
            follow_rebuilds: AtomicBool::new(false),
            interval: RwLock::new(INTERVAL),
            origins: RwLock::new(default_origins(std::env::var(ORIGIN_OVERRIDE).ok().as_deref())),
            shared: Default::default(),
            locations: Mutex::new(HashMap::new()),
            subscribed: std::sync::Once::new(),
            me: me.clone(),
        })
    }

    /// Discovery runs in this process (a server): locations that boot from now on start their plugins.
    pub fn enable(&self) {
        self.enabled.store(true, Ordering::SeqCst);
    }

    /// Configuration reloads arrive through [`LocalProviders::reconfigure`] instead of `config.updated`.
    pub fn follow_rebuilds(&self) {
        self.follow_rebuilds.store(true, Ordering::SeqCst);
    }

    /// Test-only: how often the plugins ask again.
    pub fn set_interval(&self, interval: Duration) {
        *self.interval.write() = interval;
    }

    /// Test-only: the default origin of `kind` (`None`: no default discovery).
    pub fn set_default_origin(&self, kind: Kind, origin: Option<String>) {
        self.origins.write()[index(kind)] = origin;
    }

    fn source(&self, kind: Kind, entries: &[Entry]) -> Source {
        let origin = self.origins.read()[index(kind)].clone();
        configured(kind, settings_of(entries, kind.provider_id()).as_ref(), origin.as_deref())
    }

    fn location(&self, key: &str) -> Option<Arc<Location>> {
        self.locations.lock().get(key).cloned()
    }

    /// The location's configuration (at its load and at every reload, before `config.updated`).
    pub fn observe(&self, key: &str, entries: &[Entry]) {
        let mut locations = self.locations.lock();
        if let Some(location) = locations.get(key) {
            *location.entries.write() = entries.to_vec();
            return;
        }
        let slot = |kind: Kind| Slot { source: RwLock::new(Arc::new(self.source(kind, entries))), loaded: RwLock::new((vec![], vec![])) };
        locations.insert(
            key.to_string(),
            Arc::new(Location {
                key: key.to_string(),
                entries: RwLock::new(entries.to_vec()),
                slots: Kind::ALL.map(slot),
                applying: tokio::sync::Mutex::new(()),
                started: AtomicBool::new(false),
                booting: AtomicBool::new(false),
                first_round: tokio::sync::watch::channel(0).0,
                tasks: Mutex::new(Vec::new()),
            }),
        );
    }

    /// The location starts booting: the plugins start, and what they find before [`LocalProviders::booted`] is
    /// announced by the boot itself.
    pub fn boot(&self, key: &str) {
        if !self.enabled.load(Ordering::SeqCst) {
            return;
        }
        if let Some(location) = self.location(key)
            && !location.started.load(Ordering::SeqCst)
        {
            location.booting.store(true, Ordering::SeqCst);
        }
        self.start(key);
    }

    /// Waits (at most `budget`) until every plugin of a booting location finished its first round, so what a
    /// server answering at once serves is part of the boot's notifications.
    pub async fn first_round(&self, key: &str, budget: Duration) {
        let Some(location) = self.location(key).filter(|l| l.booting.load(Ordering::SeqCst)) else { return };
        let mut rx = location.first_round.subscribe();
        let _ = tokio::time::timeout(budget, rx.wait_for(|n| *n >= Kind::ALL.len())).await;
    }

    /// The location announced its boot: later changes announce themselves.
    pub async fn booted(&self, key: &str) {
        if let Some(location) = self.location(key) {
            let _applying = location.applying.lock().await;
            location.booting.store(false, Ordering::SeqCst);
        }
        self.start(key);
    }

    /// The location's configuration reload announced itself: each plugin whose endpoint or key changed reloads.
    pub fn reconfigure(&self, key: &str) {
        let Some(location) = self.location(key).filter(|l| l.started.load(Ordering::SeqCst)) else { return };
        let Some(me) = self.me.upgrade() else { return };
        for kind in Kind::ALL {
            let (me, location) = (me.clone(), location.clone());
            tokio::spawn(async move { me.config_reload(&location, kind).await });
        }
    }

    /// The location booted: each plugin asks its server now, then every interval.
    pub fn start(&self, key: &str) {
        if !self.enabled.load(Ordering::SeqCst) {
            return;
        }
        let Some(location) = self.location(key) else { return };
        if location.started.swap(true, Ordering::SeqCst) {
            return;
        }
        let Ok(runtime) = tokio::runtime::Handle::try_current() else { return };
        self.subscribe(&runtime);
        let mut tasks = location.tasks.lock();
        for kind in Kind::ALL {
            let (me, weak) = (self.me.clone(), Arc::downgrade(&location));
            tasks.push(runtime.spawn(async move {
                let mut first = true;
                loop {
                    let (Some(me), Some(location)) = (me.upgrade(), weak.upgrade()) else { return };
                    me.refresh(&location, kind).await;
                    if std::mem::take(&mut first) {
                        location.first_round.send_modify(|n| *n += 1);
                    }
                    let interval = *me.interval.read();
                    drop((me, location));
                    tokio::time::sleep(interval).await;
                }
            }));
        }
    }

    /// The location closed: its plugins stop.
    pub fn close(&self, key: &str) {
        self.locations.lock().remove(key);
    }

    /// The discovered providers of location `key` the catalog applies (non-empty inventories, plugin order).
    pub fn discovered(&self, key: &str) -> Vec<Discovered> {
        let Some(location) = self.location(key) else { return vec![] };
        Kind::ALL
            .into_iter()
            .zip(&location.slots)
            .filter_map(|(kind, slot)| {
                let models = slot.loaded.read().1.clone();
                if models.is_empty() {
                    return None;
                }
                let source = slot.source.read().clone();
                Some(Discovered { kind, base_url: source.base_url.clone(), api_key: source.api_key.clone(), models })
            })
            .collect()
    }

    fn enabled_at(&self, key: &str, kind: Kind) -> bool {
        self.plugins.upgrade().is_some_and(|p| p.builtin_enabled(key, kind.plugin_id()))
    }

    /// `ctx.integration.reload()` then `ctx.provider.reload()`, after recording the integrations the location
    /// announced (taken before the change).
    async fn before_reload(&self, key: &str) {
        if let Some(plugins) = self.plugins.upgrade() {
            plugins.baseline(key, "integration").await;
        }
    }

    async fn reload(&self, key: &str) {
        if let Some(plugins) = self.plugins.upgrade() {
            plugins.integrations_changed(key);
            plugins.provider_reload(key);
            plugins.flush_notices(key).await;
        }
    }

    /// `refresh()`: asks the server (or the cache) and reloads when the inventory changed.
    async fn refresh(&self, location: &Location, kind: Kind) {
        if !self.enabled_at(&location.key, kind) {
            return;
        }
        let slot = &location.slots[index(kind)];
        let source = slot.source.read().clone();
        let models = match self.discover(kind, &source).await {
            Ok(Some(models)) => models,
            Ok(None) => return,
            Err(cause) => {
                tracing::debug!(provider = kind.provider_id(), %cause, "local model discovery failed");
                return;
            }
        };
        let _applying = location.applying.lock().await;
        if !Arc::ptr_eq(&source, &slot.source.read()) || slot.loaded.read().0 == models {
            return;
        }
        let booting = location.booting.load(Ordering::SeqCst);
        if !booting {
            self.before_reload(&location.key).await;
        }
        let mapped = models.iter().map(|item| model(kind, item)).collect();
        *slot.loaded.write() = (models, mapped);
        if !booting {
            self.reload(&location.key).await;
        }
    }

    /// `discover()`: the cached answer for the source's root when it is recent and for the same key, else a new
    /// request (one at a time per plugin). `Ok(None)`: nothing to discover, or no answer yet.
    async fn discover(&self, kind: Kind, source: &Source) -> Result<Option<Vec<Value>>, String> {
        let Some(root) = source.root.clone() else { return Ok(None) };
        let shared = &self.shared[index(kind)];
        let _permit = shared.lock.lock().await;
        let interval = *self.interval.read();
        {
            let mut discovery = shared.discovery.lock();
            let cached = discovery.get(&root);
            if let Some(cached) = cached
                && cached.api_key == source.api_key
                && cached.checked.elapsed() < interval
            {
                return Ok(cached.models.clone());
            }
            let models = cached.filter(|c| c.api_key == source.api_key).and_then(|c| c.models.clone());
            discovery.insert(root.clone(), Cached { checked: Instant::now(), api_key: source.api_key.clone(), models });
        }
        let client = Client { http: &self.http, source };
        let models = match kind {
            Kind::LmStudio => lmstudio(&client).await?,
            Kind::Ollama => self.ollama(&client, &root).await?,
            Kind::Vllm => vllm(&client).await?,
        };
        shared.discovery.lock().insert(root, Cached { checked: Instant::now(), api_key: source.api_key.clone(), models: Some(models.clone()) });
        Ok(Some(models))
    }

    /// Ollama: the tags, then each model's `/api/show` answer unless its digest is unchanged; models whose answer
    /// failed are left out (and asked again next time).
    async fn ollama(&self, client: &Client<'_>, root: &str) -> Result<Vec<Value>, String> {
        let shows = &self.shared[index(Kind::Ollama)].shows;
        let previous = {
            let mut shows = shows.lock();
            let previous = shows.get(root).filter(|s| s.api_key == client.source.api_key).map(|s| s.entries.clone()).unwrap_or_default();
            shows.insert(root.to_string(), Shows { api_key: client.source.api_key.clone(), entries: previous.clone() });
            previous
        };
        let response = client.get_json("/api/tags", &ollama_tags()).await?;
        let mut summaries: Vec<Value> =
            response["models"].as_array().cloned().unwrap_or_default().into_iter().filter(|m| !str_of(m, "model").is_empty()).collect();
        sort_by_key(&mut summaries, "model");
        let show_schema = ollama_show();
        let results: Vec<Option<(String, String, Value)>> = futures::stream::iter(summaries.into_iter().map(|summary| {
            let (previous, show_schema) = (&previous, &show_schema);
            async move {
                let name = str_of(&summary, "model").to_string();
                let digest = str_of(&summary, "digest").to_string();
                let info = match previous.get(&name) {
                    Some((saved, info)) if *saved == digest => info.clone(),
                    _ => client.post_json("/api/show", serde_json::json!({ "model": name }), show_schema).await.ok()?,
                };
                let mut item = summary;
                item["show"] = info;
                Some((name, digest, item))
            }
        }))
        .buffered(4)
        .collect()
        .await;
        let mut entries = HashMap::new();
        let mut models = Vec::new();
        for (name, digest, item) in results.into_iter().flatten() {
            entries.insert(name, (digest, item["show"].clone()));
            if has(item["show"].get("capabilities"), "completion") == Some(true) {
                models.push(item);
            }
        }
        shows.lock().insert(root.to_string(), Shows { api_key: client.source.api_key.clone(), entries });
        Ok(models)
    }

    /// `config.updated`: each plugin whose endpoint or key changed forgets its inventory, reloads, and asks again.
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
                    if event.kind != "config.updated" {
                        continue;
                    }
                    let Some(me) = me.upgrade() else { return };
                    if me.follow_rebuilds.load(Ordering::SeqCst) {
                        continue;
                    }
                    let Some(directory) = event.location.as_ref().map(|l| crate::location::canonical_directory(&l.directory)) else { continue };
                    let targets: Vec<Arc<Location>> = me
                        .locations
                        .lock()
                        .values()
                        .filter(|l| l.started.load(Ordering::SeqCst) && crate::location::canonical_directory(&l.key) == directory)
                        .cloned()
                        .collect();
                    for location in targets {
                        for kind in Kind::ALL {
                            let (me, location) = (me.clone(), location.clone());
                            tokio::spawn(async move { me.config_reload(&location, kind).await });
                        }
                    }
                }
            });
        });
    }

    /// `reload()` of one plugin.
    async fn config_reload(&self, location: &Location, kind: Kind) {
        if !self.enabled_at(&location.key, kind) {
            return;
        }
        let slot = &location.slots[index(kind)];
        {
            let _applying = location.applying.lock().await;
            let next = self.source(kind, &location.entries.read());
            {
                let current = slot.source.read();
                if next.base_url == current.base_url && next.api_key == current.api_key && next.root == current.root {
                    return;
                }
            }
            self.before_reload(&location.key).await;
            *slot.source.write() = Arc::new(next);
            *slot.loaded.write() = (vec![], vec![]);
            self.reload(&location.key).await;
        }
        self.refresh(location, kind).await;
    }
}

/// LM Studio: the `llm` models with a key, by key.
async fn lmstudio(client: &Client<'_>) -> Result<Vec<Value>, String> {
    let response = client.get_json("/api/v1/models", &lmstudio_response()).await?;
    let mut models: Vec<Value> = response["models"]
        .as_array()
        .cloned()
        .unwrap_or_default()
        .into_iter()
        .filter(|m| str_of(m, "type") == "llm" && !str_of(m, "key").is_empty())
        .collect();
    sort_by_key(&mut models, "key");
    Ok(models)
}

/// vLLM: once `/health` answers, the models it owns, by id.
async fn vllm(client: &Client<'_>) -> Result<Vec<Value>, String> {
    client.get("/health").await?;
    let response = client.get_json("/v1/models", &vllm_response()).await?;
    let mut models: Vec<Value> = response["data"]
        .as_array()
        .cloned()
        .unwrap_or_default()
        .into_iter()
        .filter(|m| str_of(m, "owned_by") == "vllm" && !str_of(m, "id").is_empty())
        .collect();
    sort_by_key(&mut models, "id");
    Ok(models)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn origin_override() {
        let defaults = default_origins(None);
        assert_eq!(defaults[2].as_deref(), Some("http://127.0.0.1:8000"));
        assert_eq!(default_origins(Some("http://127.0.0.1:9")), [Some("http://127.0.0.1:9".to_string()), Some("http://127.0.0.1:9".into()), Some("http://127.0.0.1:9".into())]);
        assert_eq!(default_origins(Some("off")), [None, None, None]);
        let mixed = default_origins(Some("vllm=off, ollama=http://127.0.0.1:7"));
        assert_eq!(mixed, [Some("http://127.0.0.1:1234".to_string()), Some("http://127.0.0.1:7".into()), None]);
    }

    #[test]
    fn decodes_like_the_schemas() {
        // Excess keys are dropped, a wrong type anywhere fails the whole answer.
        let ok = json!({"models": [{"type": "llm", "key": "k", "display_name": "K", "loaded_instances": [], "max_context_length": 4096, "extra": 1}]});
        let decoded = decode(&lmstudio_response(), &ok).unwrap();
        assert_eq!(decoded, json!({"models": [{"type": "llm", "key": "k", "display_name": "K", "loaded_instances": [], "max_context_length": 4096}]}));
        let bad = json!({"models": [{"type": "vision", "key": "k", "display_name": "K", "loaded_instances": [], "max_context_length": 1}]});
        assert!(decode(&lmstudio_response(), &bad).is_err());
        let fraction = json!({"models": [{"type": "llm", "key": "k", "display_name": "K", "loaded_instances": [], "max_context_length": 1.5}]});
        assert!(decode(&lmstudio_response(), &fraction).is_err());
        // Optional keys may be absent, not null; `NullOr` keys must be present.
        let null_caps = json!({"models": [{"type": "llm", "key": "k", "display_name": "K", "loaded_instances": [], "max_context_length": 1, "capabilities": null}]});
        assert!(decode(&lmstudio_response(), &null_caps).is_err());
        assert!(decode(&vllm_response(), &json!({"data": [{"id": "a", "owned_by": "vllm"}]})).is_err());
        assert!(decode(&vllm_response(), &json!({"data": [{"id": "a", "owned_by": "vllm", "max_model_len": null}]})).is_ok());
    }

    #[test]
    fn maps_models() {
        let lm = json!({"type": "llm", "key": "g", "display_name": "", "architecture": null, "loaded_instances": [{"config": {"context_length": 32768}}, {"config": {"context_length": 16384}}], "max_context_length": 262144, "capabilities": {"vision": true, "trained_for_tool_use": false}});
        assert_eq!(
            model(Kind::LmStudio, &lm),
            LocalModel { id: "g".into(), name: Some("g".into()), family: None, tools: Some(false), vision: Some(true), context: Some(16384.0) }
        );
        let ollama = json!({"name": "gemma3:4b", "model": "gemma3:4b", "details": {"family": "gemma3"}, "show": {"capabilities": ["completion", "tools", "vision"], "model_info": {"general.architecture": "gemma3", "gemma3.context_length": 131072}}});
        assert_eq!(
            model(Kind::Ollama, &ollama),
            LocalModel { id: "gemma3:4b".into(), name: Some("gemma3:4b".into()), family: Some("gemma3".into()), tools: Some(true), vision: Some(true), context: Some(131072.0) }
        );
        let vllm = json!({"id": "Qwen/Qwen3-Coder", "owned_by": "vllm", "max_model_len": null});
        assert_eq!(model(Kind::Vllm, &vllm), LocalModel { id: "Qwen/Qwen3-Coder".into(), ..Default::default() });
    }
}
