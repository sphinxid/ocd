//! models.dev catalog: fetch, cache, bundled snapshot and normalization
//! (`packages/core/src/models-dev.ts`, `packages/core/src/plugin/models-dev.ts`).

use super::native::{native, NativeContext};
use super::types::{
    aisdk, compatibility_from_interleaved, headers_from_value, supports_text, Activation, Capabilities, Cost, CostCache,
    CostTier, Limit, ModelInfo, ModelStatus, ModelTime, ProviderInfo, Variant,
};
use super::variant::{self, Support};
use crate::json::Object;
use async_trait::async_trait;
use indexmap::IndexMap;
use serde_json::{json, Value};
use std::path::PathBuf;
use std::sync::{Arc, OnceLock};
use std::time::Duration;

/// Default catalog source.
pub const DEFAULT_SOURCE: &str = "https://models.opencode.ai";
/// KV key for the default source.
pub const DEFAULT_CACHE_KEY: &str = "models-dev:catalog";
/// Cache freshness and refresh interval.
pub const TTL: Duration = Duration::from_secs(5 * 60);
/// Fetch timeout (covers retries).
pub const FETCH_TIMEOUT: Duration = Duration::from_secs(10);

/// Bundled `api.json` snapshot.
pub const SNAPSHOT: &str = include_str!("../../../../assets/models-dev/snapshot.txt");

/// One normalized provider (`ModelsDev.Snapshot`).
#[derive(Debug, Clone, PartialEq)]
pub struct Snapshot {
    pub info: ProviderInfo,
    pub models: Vec<ModelInfo>,
    pub environment: Vec<String>,
}

/// `cacheKey(source)`.
pub fn cache_key(source: &str) -> String {
    if source == DEFAULT_SOURCE {
        return DEFAULT_CACHE_KEY.to_string();
    }
    format!("models-dev:catalog:{}", oc_util::hash::fast(source))
}

/// `bodyDigest(text)`: SHA-256 hex.
pub fn body_digest(text: &str) -> String {
    oc_util::hash::sha256(text)
}

// ---------------------------------------------------------------------------------------------
// Normalization
// ---------------------------------------------------------------------------------------------

fn s<'a>(v: &'a Value, key: &str) -> Option<&'a str> {
    v.get(key).and_then(Value::as_str)
}

fn native_package(provider: &Value, model: Option<&Value>) -> String {
    let provider_id = s(provider, "id").unwrap_or("");
    let model_npm = model.and_then(|m| m.get("provider")).and_then(|p| s(p, "npm"));
    let npm = model_npm.or_else(|| s(provider, "npm")).unwrap_or("");
    let context = NativeContext {
        provider_id,
        model_id: model.and_then(|m| s(m, "id")),
        shape: model.and_then(|m| m.get("provider")).and_then(|p| s(p, "shape")),
        ..Default::default()
    };
    native(npm, &context).unwrap_or_else(|| aisdk(npm))
}

/// `Date.parse(date)` for the formats models.dev uses; `0` when unparseable.
pub fn released(date: &str) -> f64 {
    use chrono::{NaiveDate, NaiveDateTime, TimeZone, Utc};
    let date = date.trim();
    let utc_date = |y: i32, m: u32, d: u32| {
        NaiveDate::from_ymd_opt(y, m, d).map(|nd| Utc.from_utc_datetime(&nd.and_hms_opt(0, 0, 0).unwrap()).timestamp_millis() as f64)
    };
    let parts: Vec<&str> = date.split('-').collect();
    let all_digits = |p: &str| !p.is_empty() && p.chars().all(|c| c.is_ascii_digit());
    match parts.as_slice() {
        [y] if y.len() == 4 && all_digits(y) => return utc_date(y.parse().unwrap(), 1, 1).unwrap_or(0.0),
        [y, m] if y.len() == 4 && m.len() == 2 && all_digits(y) && all_digits(m) => {
            return utc_date(y.parse().unwrap(), m.parse().unwrap(), 1).unwrap_or(0.0);
        }
        [y, m, d] if y.len() == 4 && m.len() == 2 && d.len() == 2 && all_digits(y) && all_digits(m) && all_digits(d) => {
            return utc_date(y.parse().unwrap(), m.parse().unwrap(), d.parse().unwrap()).unwrap_or(0.0);
        }
        _ => {}
    }
    if let Ok(dt) = chrono::DateTime::parse_from_rfc3339(date) {
        return dt.timestamp_millis() as f64;
    }
    // Date-time forms without an offset are local time in JavaScript.
    for format in ["%Y-%m-%dT%H:%M:%S%.f", "%Y-%m-%dT%H:%M"] {
        if let Ok(naive) = NaiveDateTime::parse_from_str(date, format) {
            if let Some(local) = chrono::Local.from_local_datetime(&naive).earliest() {
                return local.timestamp_millis() as f64;
            }
        }
    }
    0.0
}

fn f(v: Option<&Value>) -> f64 {
    v.and_then(Value::as_f64).unwrap_or(0.0)
}

/// `cost(input)`: base entry plus context tiers (missing values = 0).
fn cost(input: Option<&Value>) -> Vec<Cost> {
    let base = Cost {
        tier: None,
        input: f(input.and_then(|c| c.get("input"))),
        output: f(input.and_then(|c| c.get("output"))),
        cache: CostCache { read: f(input.and_then(|c| c.get("cache_read"))), write: f(input.and_then(|c| c.get("cache_write"))) },
    };
    let mut out = vec![base];
    if let Some(tiers) = input.and_then(|c| c.get("tiers")).and_then(Value::as_array) {
        for item in tiers {
            let tier = item.get("tier").and_then(|t| {
                Some(CostTier { kind: t.get("type").and_then(Value::as_str).unwrap_or("context").into(), size: t.get("size")?.as_f64()? as i64 })
            });
            out.push(Cost {
                tier,
                input: f(item.get("input")),
                output: f(item.get("output")),
                cache: CostCache { read: f(item.get("cache_read")), write: f(item.get("cache_write")) },
            });
        }
    }
    out
}

/// `mergeCost(base, override)`.
fn merge_cost(base: &[Cost], override_: Option<&Value>) -> Vec<Cost> {
    let Some(override_) = override_ else { return base.to_vec() };
    let next = cost(Some(override_));
    let merge = |left: &Cost, right: &Cost| Cost {
        tier: right.tier.clone().or_else(|| left.tier.clone()),
        input: right.input,
        output: right.output,
        cache: right.cache,
    };
    let key = |c: &Cost| match &c.tier {
        Some(t) => format!("{}:{}", t.kind, t.size),
        None => "base:0".to_string(),
    };
    let mut tiers: IndexMap<String, Cost> = base.iter().skip(1).map(|c| (key(c), c.clone())).collect();
    for item in next.iter().skip(1) {
        let k = key(item);
        let merged = match tiers.get(&k) {
            Some(current) => merge(current, item),
            None => item.clone(),
        };
        tiers.insert(k, merged);
    }
    let default = base.first().cloned().unwrap_or_default();
    let mut out = vec![merge(&default, &next[0])];
    out.extend(tiers.into_values());
    out
}

fn mode_name(name: &str, mode: &str) -> String {
    let mut chars = mode.chars();
    let upper = match chars.next() {
        Some(c) => c.to_uppercase().collect::<String>() + chars.as_str(),
        None => String::new(),
    };
    format!("{name} {upper}")
}

fn supports(model: &Value) -> Vec<Support> {
    model
        .get("reasoning_options")
        .and_then(Value::as_array)
        .map(|items| items.iter().filter_map(variant::support_from_json).collect())
        .unwrap_or_default()
}

fn strings(v: Option<&Value>) -> Vec<String> {
    v.and_then(Value::as_array).map(|a| a.iter().filter_map(|s| s.as_str().map(String::from)).collect()).unwrap_or_default()
}

struct ModelInput<'a> {
    name: Option<String>,
    cost: Option<Vec<Cost>>,
    request: Option<&'a Value>,
    variants: Option<Vec<Variant>>,
}

fn model_info(provider: &Value, id: &str, model: &Value, input: ModelInput<'_>) -> ModelInfo {
    let provider_id = s(provider, "id").unwrap_or("").to_string();
    let resolved = native_package(provider, Some(model));
    let model_provider = model.get("provider");
    let model_npm = model_provider.and_then(|p| s(p, "npm")).filter(|n| !n.is_empty());
    let package = if model_npm.is_some() || resolved != native_package(provider, None) { Some(resolved.clone()) } else { None };
    let mut settings = Object::new();
    if let Some(api) = model_provider.and_then(|p| s(p, "api")).filter(|a| !a.is_empty()) {
        settings.insert("baseURL".into(), json!(api));
    }
    if resolved == "@opencode/ai/providers/openai-compatible" {
        settings.insert("provider".into(), json!(provider_id));
    }
    let limit = model.get("limit");
    let modalities = model.get("modalities");
    ModelInfo {
        id: id.to_string(),
        model_id: s(model, "id").unwrap_or("").to_string(),
        provider_id,
        canonical: None,
        family: s(model, "family").filter(|f| !f.is_empty()).map(String::from),
        name: input.name.unwrap_or_else(|| s(model, "name").unwrap_or("").to_string()),
        compatibility: model.get("interleaved").and_then(compatibility_from_interleaved),
        package,
        settings: if settings.is_empty() { None } else { Some(settings) },
        headers: input.request.and_then(|r| r.get("headers")).and_then(headers_from_value),
        body: input.request.and_then(|r| r.get("body")).and_then(Value::as_object).cloned(),
        capabilities: Capabilities {
            tools: model.get("tool_call").and_then(Value::as_bool).unwrap_or(false),
            input: strings(modalities.and_then(|m| m.get("input"))),
            output: strings(modalities.and_then(|m| m.get("output"))),
        },
        variants: input.variants.unwrap_or_default(),
        time: ModelTime { released: s(model, "release_date").map(released).unwrap_or(0.0) },
        cost: input.cost.unwrap_or_else(|| cost(model.get("cost"))),
        status: s(model, "status").and_then(ModelStatus::parse).unwrap_or(ModelStatus::Active),
        enabled: true,
        limit: Limit {
            context: f(limit.and_then(|l| l.get("context"))) as i64,
            input: limit.and_then(|l| l.get("input")).and_then(Value::as_f64).map(|v| v as i64),
            output: f(limit.and_then(|l| l.get("output"))) as i64,
        },
    }
}

/// `normalize(api.json)`: models.dev providers → catalog snapshots.
pub fn normalize(input: &Object) -> Vec<Snapshot> {
    let mut providers = vec![];
    for item in input.values() {
        if !item.is_object() {
            continue;
        }
        let provider_id = s(item, "id").unwrap_or("").to_string();
        let package = native_package(item, None);
        let settings = match s(item, "api") {
            Some(api) if !api.is_empty() && package != "@opencode/ai/providers/cloudflare-workers-ai" => {
                let mut o = Object::new();
                o.insert("baseURL".into(), json!(api));
                Some(o)
            }
            _ => None,
        };
        let info = ProviderInfo {
            id: provider_id.clone(),
            canonical: None,
            integration_id: None,
            name: s(item, "name").unwrap_or("").to_string(),
            activation: Activation::Auto,
            package,
            settings,
            headers: None,
            body: None,
        };
        let mut models = vec![];
        if let Some(entries) = item.get("models").and_then(Value::as_object) {
            for model in entries.values() {
                if !model.is_object() {
                    continue;
                }
                let base_cost = cost(model.get("cost"));
                let id = s(model, "id").unwrap_or("").to_string();
                let mut base = model_info(item, &id, model, ModelInput { name: None, cost: Some(base_cost.clone()), request: None, variants: None });
                let mut probe = base.clone();
                probe.package = Some(native_package(item, Some(model)));
                let variants = variant::resolve(&probe, &supports(model));
                base.variants = variants.clone();
                models.push(base);
                if let Some(modes) = model.get("experimental").and_then(|e| e.get("modes")).and_then(Value::as_object) {
                    for (mode, options) in modes {
                        models.push(model_info(
                            item,
                            &format!("{id}-{mode}"),
                            model,
                            ModelInput {
                                name: Some(mode_name(s(model, "name").unwrap_or(""), mode)),
                                cost: Some(merge_cost(&base_cost, options.get("cost"))),
                                request: options.get("provider"),
                                variants: Some(variants.clone()),
                            },
                        ));
                    }
                }
            }
        }
        providers.push(Snapshot { info, models, environment: strings(item.get("env")) });
    }
    providers
}

/// Decode `api.json` text as a JSON record.
pub fn decode_catalog(text: &str) -> Result<Object, String> {
    match serde_json::from_str::<Value>(text) {
        Ok(Value::Object(o)) => Ok(o),
        Ok(_) => Err("Expected an object".into()),
        Err(e) => Err(e.to_string()),
    }
}

/// Bedrock entries that require inference profiles on Bedrock Runtime.
pub const BEDROCK_PROFILE_ONLY_IDS: &[&str] = &[
    "amazon.nova-2-lite-v1:0",
    "anthropic.claude-fable-5",
    "anthropic.claude-fable-5-1",
    "anthropic.claude-haiku-4-5-20251001-v1:0",
    "anthropic.claude-opus-4-1-20250805-v1:0",
    "anthropic.claude-opus-4-5-20251101-v1:0",
    "anthropic.claude-opus-4-7",
    "anthropic.claude-opus-4-8",
    "anthropic.claude-opus-5",
    "anthropic.claude-sonnet-4-5-20250929-v1:0",
    "anthropic.claude-sonnet-5",
    "deepseek.r1-v1:0",
    "mistral.pixtral-large-2502-v1:0",
];

/// Providers replaced by canonical Azure / Google Vertex.
pub const DROPPED_PROVIDERS: &[&str] = &["azure-cognitive-services", "google-vertex-anthropic"];

/// `snapshots(data)` of the models.dev plugin: drop legacy aliases, deprecated, non-text and
/// Bedrock profile-only models.
pub fn filter_snapshots(data: &[Snapshot]) -> Vec<Snapshot> {
    data.iter()
        .filter(|p| !DROPPED_PROVIDERS.contains(&p.info.id.as_str()))
        .map(|p| Snapshot {
            info: p.info.clone(),
            environment: p.environment.clone(),
            models: p
                .models
                .iter()
                .filter(|m| {
                    m.status != ModelStatus::Deprecated
                        && supports_text(&m.capabilities)
                        && !(p.info.id == "amazon-bedrock" && BEDROCK_PROFILE_ONLY_IDS.contains(&m.model_id.as_str()))
                })
                .cloned()
                .collect(),
        })
        .collect()
}

static BUNDLED: OnceLock<Arc<Vec<Snapshot>>> = OnceLock::new();

/// The bundled snapshot, decoded and normalized once per process.
pub fn bundled() -> Arc<Vec<Snapshot>> {
    BUNDLED
        .get_or_init(|| Arc::new(decode_catalog(SNAPSHOT).map(|c| normalize(&c)).unwrap_or_default()))
        .clone()
}

// ---------------------------------------------------------------------------------------------
// Cache + HTTP abstractions
// ---------------------------------------------------------------------------------------------

/// KV storage for the cached catalog (`KV.Service`; SQLite `kv` table in production).
#[async_trait]
pub trait CatalogCache: Send + Sync {
    async fn get(&self, key: &str) -> Option<Value>;
    async fn set(&self, key: &str, value: Value) -> Result<(), String>;
    async fn remove(&self, key: &str);
}

/// In-memory cache.
#[derive(Default)]
pub struct MemoryCache {
    pub values: parking_lot::Mutex<IndexMap<String, Value>>,
}

#[async_trait]
impl CatalogCache for MemoryCache {
    async fn get(&self, key: &str) -> Option<Value> {
        self.values.lock().get(key).cloned()
    }
    async fn set(&self, key: &str, value: Value) -> Result<(), String> {
        self.values.lock().insert(key.to_string(), value);
        Ok(())
    }
    async fn remove(&self, key: &str) {
        self.values.lock().shift_remove(key);
    }
}

/// A raw HTTP response.
#[derive(Debug, Clone)]
pub struct HttpResponse {
    pub status: u16,
    pub body: String,
}

/// `GET url` with a `User-Agent` header (one attempt; retries are applied by [`ModelsDev`]).
#[async_trait]
pub trait HttpGet: Send + Sync {
    async fn get(&self, url: &str, user_agent: &str) -> Result<HttpResponse, String>;
}

/// reqwest-backed [`HttpGet`].
pub struct ReqwestGet(pub reqwest::Client);

impl Default for ReqwestGet {
    fn default() -> Self {
        Self(reqwest::Client::new())
    }
}

#[async_trait]
impl HttpGet for ReqwestGet {
    async fn get(&self, url: &str, user_agent: &str) -> Result<HttpResponse, String> {
        let response = self.0.get(url).header("User-Agent", user_agent).send().await.map_err(|e| e.to_string())?;
        let status = response.status().as_u16();
        let body = response.text().await.map_err(|e| e.to_string())?;
        Ok(HttpResponse { status, body })
    }
}

/// `ModelsDev.Options`.
#[derive(Debug, Clone)]
pub struct ModelsDevOptions {
    /// Source origin (`OPENCODE_MODELS_URL`); empty → default.
    pub url: Option<String>,
    /// Local `api.json` override (`OPENCODE_MODELS_PATH`).
    pub file: Option<PathBuf>,
    /// Network fetch enabled (`!OPENCODE_DISABLE_MODELS_FETCH`).
    pub fetch: bool,
    /// Use the bundled snapshot as boot floor.
    pub snapshot: bool,
    /// `opencode/<channel>/<version>/<name>`.
    pub user_agent: String,
}

impl Default for ModelsDevOptions {
    fn default() -> Self {
        Self { url: None, file: None, fetch: true, snapshot: true, user_agent: "opencode/unknown/unknown/opencode".into() }
    }
}

fn env_truthy(value: Option<String>) -> bool {
    value.map(|v| v == "1" || v.to_lowercase() == "true").unwrap_or(false)
}

impl ModelsDevOptions {
    /// Options from `OPENCODE_MODELS_URL`, `OPENCODE_MODELS_PATH`, `OPENCODE_DISABLE_MODELS_FETCH`.
    pub fn from_env(user_agent: impl Into<String>) -> Self {
        Self {
            url: std::env::var("OPENCODE_MODELS_URL").ok(),
            file: std::env::var("OPENCODE_MODELS_PATH").ok().map(PathBuf::from),
            fetch: !env_truthy(std::env::var("OPENCODE_DISABLE_MODELS_FETCH").ok()),
            snapshot: true,
            user_agent: user_agent.into(),
        }
    }

    pub fn source(&self) -> &str {
        match self.url.as_deref() {
            Some(u) if !u.is_empty() => u,
            _ => DEFAULT_SOURCE,
        }
    }
}

/// `App.useragent`.
pub fn user_agent(channel: &str, version: &str, name: &str) -> String {
    format!("opencode/{channel}/{version}/{name}")
}

/// Fetch / populate failure.
#[derive(Debug, Clone, PartialEq, thiserror::Error)]
#[error("{0}")]
pub struct ModelsDevError(pub String);

struct Cached {
    catalog: Object,
    updated_at: f64,
    digest: Option<String>,
}

fn now_ms() -> f64 {
    chrono::Utc::now().timestamp_millis() as f64
}

/// Transient statuses retried by `HttpClient.retryTransient`.
fn transient(status: u16) -> bool {
    matches!(status, 408 | 429 | 500 | 502 | 503 | 504)
}

/// The models.dev service: memoized catalog, cache, refresh.
pub struct ModelsDev {
    options: ModelsDevOptions,
    cache: Arc<dyn CatalogCache>,
    http: Arc<dyn HttpGet>,
    key: String,
    memo: parking_lot::Mutex<Option<Arc<Vec<Snapshot>>>>,
    populating: tokio::sync::Mutex<()>,
    lock: tokio::sync::Mutex<()>,
    events: tokio::sync::broadcast::Sender<()>,
}

impl ModelsDev {
    pub fn new(options: ModelsDevOptions, cache: Arc<dyn CatalogCache>, http: Arc<dyn HttpGet>) -> Self {
        let key = cache_key(options.source());
        let (events, _) = tokio::sync::broadcast::channel(16);
        Self { options, cache, http, key, memo: parking_lot::Mutex::new(None), populating: tokio::sync::Mutex::new(()), lock: tokio::sync::Mutex::new(()), events }
    }

    /// Production service with the reqwest client.
    pub fn with_reqwest(options: ModelsDevOptions, cache: Arc<dyn CatalogCache>) -> Self {
        Self::new(options, cache, Arc::new(ReqwestGet::default()))
    }

    pub fn cache_key(&self) -> &str {
        &self.key
    }

    pub fn options(&self) -> &ModelsDevOptions {
        &self.options
    }

    /// `ModelsDev.Event.Refreshed` notifications.
    pub fn subscribe(&self) -> tokio::sync::broadcast::Receiver<()> {
        self.events.subscribe()
    }

    async fn load_from_cache(&self) -> Option<Cached> {
        let value = self.cache.get(&self.key).await?;
        let decoded = (|| {
            let o = value.as_object()?;
            let updated_at = o.get("updatedAt")?.as_f64()?;
            let digest = match o.get("digest") {
                None => None,
                Some(Value::String(d)) => Some(d.clone()),
                Some(_) => return None,
            };
            let catalog = decode_catalog(o.get("body")?.as_str()?).ok()?;
            Some(Cached { catalog, updated_at, digest })
        })();
        if decoded.is_none() {
            self.cache.remove(&self.key).await;
        }
        decoded
    }

    /// `GET <source>/api.json`: 2 retries on transport errors / transient statuses (jittered exponential
    /// backoff from 200 ms), non-2xx fails, everything within 10 s.
    pub async fn fetch_api(&self) -> Result<String, ModelsDevError> {
        let url = format!("{}/api.json", self.options.source());
        let attempt = async {
            let mut delay = 200.0f64;
            let mut retries = 0;
            loop {
                let result = self.http.get(&url, &self.options.user_agent).await;
                let retry = match &result {
                    Err(_) => true,
                    Ok(r) => transient(r.status),
                };
                if retry && retries < 2 {
                    retries += 1;
                    let jitter = 0.8 + rand::random::<f64>() * 0.4;
                    tokio::time::sleep(Duration::from_millis((delay * jitter) as u64)).await;
                    delay *= 2.0;
                    continue;
                }
                let response = result.map_err(ModelsDevError)?;
                if !(200..300).contains(&response.status) {
                    return Err(ModelsDevError(format!("StatusCode: non 2xx status code ({} GET {url})", response.status)));
                }
                return Ok(response.body);
            }
        };
        tokio::time::timeout(FETCH_TIMEOUT, attempt).await.map_err(|_| ModelsDevError("Request timed out".into()))?
    }

    async fn write_cache(&self, text: &str, digest: Option<String>) {
        let digest = digest.unwrap_or_else(|| body_digest(text));
        let value = json!({"updatedAt": crate::json::num(now_ms()), "digest": digest, "body": text});
        if let Err(cause) = self.cache.set(&self.key, value).await {
            tracing::warn!(cause = %cause, "Failed to cache models.dev catalog");
        }
    }

    async fn fetch_and_write(&self) -> Result<Object, ModelsDevError> {
        let text = self.fetch_api().await?;
        let catalog = decode_catalog(&text).map_err(ModelsDevError)?;
        self.write_cache(&text, None).await;
        Ok(catalog)
    }

    async fn populate(&self) -> Result<Vec<Snapshot>, ModelsDevError> {
        if let Some(file) = &self.options.file {
            if let Ok(text) = tokio::fs::read_to_string(file).await {
                if let Ok(catalog) = decode_catalog(&text) {
                    return Ok(normalize(&catalog));
                }
            }
        }
        if self.options.file.is_none() {
            if let Some(cached) = self.load_from_cache().await {
                return Ok(normalize(&cached.catalog));
            }
        }
        if self.options.snapshot {
            return Ok(bundled().as_ref().clone());
        }
        if !self.options.fetch {
            return Ok(vec![]);
        }
        let _guard = self.lock.lock().await;
        if self.options.file.is_none() {
            if let Some(stored) = self.load_from_cache().await {
                return Ok(normalize(&stored.catalog));
            }
        }
        Ok(normalize(&self.fetch_and_write().await?))
    }

    /// The memoized catalog (single-flight; failures are not memoized).
    pub async fn get(&self) -> Result<Arc<Vec<Snapshot>>, ModelsDevError> {
        if let Some(value) = self.memo.lock().clone() {
            return Ok(value);
        }
        let _flight = self.populating.lock().await;
        if let Some(value) = self.memo.lock().clone() {
            return Ok(value);
        }
        let value = Arc::new(self.populate().await?);
        *self.memo.lock() = Some(value.clone());
        Ok(value)
    }

    /// Drop the memoized catalog.
    pub fn invalidate(&self) {
        *self.memo.lock() = None;
    }

    /// `refresh(force)`: skip fresh caches (< 5 min) and byte-identical bodies unless forced; errors are
    /// logged (`"Failed to fetch models.dev"`) and ignored. Returns whether the catalog changed.
    pub async fn refresh(&self, force: bool) -> bool {
        let _guard = self.lock.lock().await;
        let result: Result<bool, ModelsDevError> = async {
            let stored = self.load_from_cache().await;
            if !force {
                if let Some(stored) = &stored {
                    if now_ms() - stored.updated_at < TTL.as_millis() as f64 {
                        return Ok(false);
                    }
                }
            }
            let text = self.fetch_api().await?;
            let digest = body_digest(&text);
            if !force && stored.as_ref().and_then(|s| s.digest.as_deref()) == Some(digest.as_str()) {
                return Ok(false);
            }
            decode_catalog(&text).map_err(ModelsDevError)?;
            self.write_cache(&text, Some(digest)).await;
            self.invalidate();
            let _ = self.events.send(());
            Ok(true)
        }
        .await;
        match result {
            Ok(changed) => changed,
            Err(cause) => {
                tracing::error!(cause = %cause, "Failed to fetch models.dev");
                false
            }
        }
    }

    /// Start the refresh loop (immediately, then every 5 minutes) when fetching is enabled.
    pub fn spawn_refresh_loop(self: &Arc<Self>) -> Option<tokio::task::JoinHandle<()>> {
        if !self.options.fetch {
            return None;
        }
        let this = self.clone();
        Some(tokio::spawn(async move {
            loop {
                this.refresh(false).await;
                tokio::time::sleep(TTL).await;
            }
        }))
    }
}

// ---------------------------------------------------------------------------------------------
// Integrations
// ---------------------------------------------------------------------------------------------

/// An integration auth method registered by catalog plugins.
#[derive(Debug, Clone, PartialEq)]
pub enum IntegrationMethod {
    /// `{type: "key", label?}`.
    Key { label: Option<String> },
    /// `{type: "env", names}`.
    Env { names: Vec<String> },
}

/// Integration registration (`integrations.update` + `integrations.method.update`).
#[derive(Debug, Clone, PartialEq)]
pub struct IntegrationSpec {
    pub id: String,
    pub name: Option<String>,
    pub methods: Vec<IntegrationMethod>,
}

/// Integrations registered by the models.dev plugin: providers with env vars get a key method and an
/// env method listing every variable.
pub fn integrations(data: &[Snapshot]) -> Vec<IntegrationSpec> {
    data.iter()
        .filter(|p| !p.environment.is_empty())
        .map(|p| IntegrationSpec {
            id: p.info.id.clone(),
            name: Some(p.info.name.clone()),
            methods: vec![IntegrationMethod::Key { label: None }, IntegrationMethod::Env { names: p.environment.clone() }],
        })
        .collect()
}
