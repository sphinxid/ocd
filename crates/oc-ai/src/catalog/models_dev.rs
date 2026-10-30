//! models.dev catalog: fetch, cache, bundled snapshot and normalization
//! (`packages/core/src/models-dev.ts`, `packages/core/src/models-dev/v1.ts`,
//! `packages/core/src/plugin/models-dev.ts`). The catalog is models.dev's v2 API; v1 catalogs from
//! `OPENCODE_MODELS_PATH` or a custom mirror are converted first ([`super::models_dev_v1`]).

use super::config::{ConfigReasoning, ConfigReasoningBudget};
use super::models_dev_v1::{is_v1, to_v2};
use super::native::{native, NativeContext};
use super::types::{
    aisdk, headers_from_value, package_name, supports_text, Activation, Capabilities, Cost, CostCache, CostTier,
    HeaderMap, Limit, ModelInfo, ModelStatus, ModelTime, ProviderInfo, Variant, CATALOG_PACKAGES,
};
use super::variant;
use crate::json::Object;
use crate::types::Compatibility;
use async_trait::async_trait;
use indexmap::{IndexMap, IndexSet};
use serde_json::{json, Value};
use std::path::PathBuf;
use std::sync::{Arc, OnceLock};
use std::time::Duration;

/// Default catalog source.
pub const DEFAULT_SOURCE: &str = "https://models.opencode.ai";
/// KV key for the default source. Versioned so an entry cached from the v1 API is never read as v2.
pub const DEFAULT_CACHE_KEY: &str = "models-dev:v2:catalog";
/// Cache freshness and refresh interval.
pub const TTL: Duration = Duration::from_secs(5 * 60);
/// Fetch timeout (covers retries).
pub const FETCH_TIMEOUT: Duration = Duration::from_secs(10);

/// Bundled snapshot of the catalog at [`CATALOG_PATH`].
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
    format!("models-dev:v2:catalog:{}", oc_util::hash::fast(source))
}

/// The unversioned key the v1 catalog was cached under (removed at startup).
pub fn legacy_cache_key(key: &str) -> String {
    key.replacen("models-dev:v2:", "models-dev:", 1)
}

/// `bodyDigest(text)`: SHA-256 hex.
pub fn body_digest(text: &str) -> String {
    oc_util::hash::sha256(text)
}

// ---------------------------------------------------------------------------------------------
// Decoding: only the fields opencode reads are decoded. Unknown keys are ignored, and a model that
// doesn't match is skipped rather than failing the whole catalog.
// ---------------------------------------------------------------------------------------------

/// The model types opencode keeps (`TYPES`). Only chat models are offered for sessions; the others
/// are listed on request.
pub const TYPES: &[&str] = &["chat", "decision", "image"];

/// `catalogPath`: the experimental v2 API, limited to the model types opencode keeps.
pub const CATALOG_PATH: &str = "/experimental/v2.0/api.json?type=chat,decision,image";

type Decoded<T> = Result<T, String>;

fn expected(path: &str, what: &str, actual: Option<&Value>) -> String {
    match actual {
        None => format!("Missing key\n  at {path}"),
        Some(value) => format!("Expected {what}, got {value}\n  at {path}"),
    }
}

fn d_object<'a>(value: &'a Value, path: &str) -> Decoded<&'a Object> {
    value.as_object().ok_or_else(|| expected(path, "object", Some(value)))
}

fn d_string(value: Option<&Value>, path: &str) -> Decoded<String> {
    match value {
        Some(Value::String(s)) => Ok(s.clone()),
        other => Err(expected(path, "string", other)),
    }
}

fn d_opt_string(value: Option<&Value>, path: &str) -> Decoded<Option<String>> {
    value.map(|v| d_string(Some(v), path)).transpose()
}

fn d_number(value: Option<&Value>, path: &str) -> Decoded<f64> {
    match value {
        Some(Value::Number(n)) => n.as_f64().ok_or_else(|| expected(path, "a finite number", value)),
        other => Err(expected(path, "a finite number", other)),
    }
}

fn d_opt_number(value: Option<&Value>, path: &str) -> Decoded<Option<f64>> {
    value.map(|v| d_number(Some(v), path)).transpose()
}

fn d_opt_int(value: Option<&Value>, path: &str) -> Decoded<Option<i64>> {
    match d_opt_number(value, path)? {
        Some(n) if n.fract() != 0.0 || n.abs() > 9_007_199_254_740_991.0 => Err(expected(path, "an integer", value)),
        other => Ok(other.map(|n| n as i64)),
    }
}

fn d_bool(value: Option<&Value>, path: &str) -> Decoded<bool> {
    match value {
        Some(Value::Bool(b)) => Ok(*b),
        other => Err(expected(path, "boolean", other)),
    }
}

fn d_opt_bool(value: Option<&Value>, path: &str) -> Decoded<Option<bool>> {
    value.map(|v| d_bool(Some(v), path)).transpose()
}

fn d_strings(value: Option<&Value>, path: &str) -> Decoded<Vec<String>> {
    match value {
        Some(Value::Array(items)) => {
            items.iter().enumerate().map(|(i, v)| d_string(Some(v), &format!("{path}[{i}]"))).collect()
        }
        other => Err(expected(path, "array", other)),
    }
}

fn d_record<'a>(value: Option<&'a Value>, path: &str) -> Decoded<&'a Object> {
    match value {
        Some(Value::Object(o)) => Ok(o),
        other => Err(expected(path, "object", other)),
    }
}

fn d_opt_record<'a>(value: Option<&'a Value>, path: &str) -> Decoded<Option<&'a Object>> {
    value.map(|v| d_record(Some(v), path)).transpose()
}

/// A cache write price: one price, or prices keyed by TTL (opencode keeps the first).
#[derive(Debug, Clone)]
enum CacheWrite {
    Price(f64),
    Ttl(Option<f64>),
}

impl CacheWrite {
    fn first(&self) -> Option<f64> {
        match self {
            CacheWrite::Price(p) => Some(*p),
            CacheWrite::Ttl(first) => *first,
        }
    }
}

fn d_cache_write(value: Option<&Value>, path: &str) -> Decoded<Option<CacheWrite>> {
    match value {
        None => Ok(None),
        Some(Value::Number(_)) => Ok(Some(CacheWrite::Price(d_number(value, path)?))),
        Some(Value::Object(o)) => {
            let mut first = None;
            for (i, (k, v)) in o.iter().enumerate() {
                let price = d_number(Some(v), &format!("{path}[\"{k}\"]"))?;
                if i == 0 {
                    first = Some(price);
                }
            }
            Ok(Some(CacheWrite::Ttl(first)))
        }
        other => Err(expected(path, "number | record", other)),
    }
}

#[derive(Debug, Clone, Default)]
struct CachePrices {
    read: Option<f64>,
    write: Option<CacheWrite>,
    explicit_write: Option<CacheWrite>,
}

fn d_cache_prices(value: Option<&Value>, path: &str) -> Decoded<Option<CachePrices>> {
    let Some(o) = d_opt_record(value, path)? else { return Ok(None) };
    let explicit_write = match d_opt_record(o.get("explicit"), &format!("{path}[\"explicit\"]"))? {
        Some(e) => d_cache_write(e.get("write"), &format!("{path}[\"explicit\"][\"write\"]"))?,
        None => None,
    };
    Ok(Some(CachePrices {
        read: d_opt_number(o.get("read"), &format!("{path}[\"read\"]"))?,
        write: d_cache_write(o.get("write"), &format!("{path}[\"write\"]"))?,
        explicit_write,
    }))
}

/// `cacheWrite(cache)`: the first TTL listed, else the explicit cache's.
fn cache_write(cache: Option<&CachePrices>) -> Option<f64> {
    let cache = cache?;
    cache.write.as_ref().and_then(CacheWrite::first).or_else(|| cache.explicit_write.as_ref().and_then(CacheWrite::first))
}

/// A number scales every price below it; an object scales only the prices it names.
#[derive(Debug, Clone)]
enum Factor {
    Num(f64),
    Map(Vec<(String, Factor)>),
}

fn d_factor(value: Option<&Value>, path: &str) -> Decoded<Factor> {
    match value {
        Some(Value::Number(_)) => Ok(Factor::Num(d_number(value, path)?)),
        Some(Value::Object(o)) => o
            .iter()
            .map(|(k, v)| Ok((k.clone(), d_factor(Some(v), &format!("{path}[\"{k}\"]"))?)))
            .collect::<Decoded<Vec<_>>>()
            .map(Factor::Map),
        other => Err(expected(path, "number | record", other)),
    }
}

#[derive(Debug, Clone)]
struct InputOverride {
    when: Object,
    price: Option<f64>,
    cache: Option<CachePrices>,
}

#[derive(Debug, Clone)]
struct SourceInput {
    unit: Option<String>,
    price: f64,
    cache: Option<CachePrices>,
    overrides: Vec<InputOverride>,
}

#[derive(Debug, Clone)]
struct OutputOverride {
    when: Object,
    price: Option<f64>,
}

#[derive(Debug, Clone)]
struct SourceOutput {
    unit: Option<String>,
    price: f64,
    overrides: Vec<OutputOverride>,
}

#[derive(Debug, Clone, Default)]
struct SourceCost {
    input: Option<SourceInput>,
    output: Option<SourceOutput>,
    multipliers: Vec<(Object, Factor)>,
}

fn d_array<'a>(value: Option<&'a Value>, path: &str) -> Decoded<Option<&'a Vec<Value>>> {
    match value {
        None => Ok(None),
        Some(Value::Array(items)) => Ok(Some(items)),
        other => Err(expected(path, "array", other)),
    }
}

fn d_cost(value: Option<&Value>, path: &str) -> Decoded<Option<SourceCost>> {
    let Some(o) = d_opt_record(value, path)? else { return Ok(None) };
    let mut out = SourceCost::default();
    let text_path = format!("{path}[\"text\"]");
    if let Some(text) = d_opt_record(o.get("text"), &text_path)? {
        let input_path = format!("{text_path}[\"input\"]");
        if let Some(input) = d_opt_record(text.get("input"), &input_path)? {
            let mut overrides = vec![];
            for (i, item) in d_array(input.get("overrides"), &format!("{input_path}[\"overrides\"]"))?.into_iter().flatten().enumerate() {
                let p = format!("{input_path}[\"overrides\"][{i}]");
                let item = d_object(item, &p)?;
                overrides.push(InputOverride {
                    when: d_record(item.get("when"), &format!("{p}[\"when\"]"))?.clone(),
                    price: d_opt_number(item.get("price"), &format!("{p}[\"price\"]"))?,
                    cache: d_cache_prices(item.get("cache"), &format!("{p}[\"cache\"]"))?,
                });
            }
            out.input = Some(SourceInput {
                unit: d_opt_string(input.get("unit"), &format!("{input_path}[\"unit\"]"))?,
                price: d_number(input.get("price"), &format!("{input_path}[\"price\"]"))?,
                cache: d_cache_prices(input.get("cache"), &format!("{input_path}[\"cache\"]"))?,
                overrides,
            });
        }
        let output_path = format!("{text_path}[\"output\"]");
        if let Some(output) = d_opt_record(text.get("output"), &output_path)? {
            let mut overrides = vec![];
            for (i, item) in d_array(output.get("overrides"), &format!("{output_path}[\"overrides\"]"))?.into_iter().flatten().enumerate() {
                let p = format!("{output_path}[\"overrides\"][{i}]");
                let item = d_object(item, &p)?;
                overrides.push(OutputOverride {
                    when: d_record(item.get("when"), &format!("{p}[\"when\"]"))?.clone(),
                    price: d_opt_number(item.get("price"), &format!("{p}[\"price\"]"))?,
                });
            }
            out.output = Some(SourceOutput {
                unit: d_opt_string(output.get("unit"), &format!("{output_path}[\"unit\"]"))?,
                price: d_number(output.get("price"), &format!("{output_path}[\"price\"]"))?,
                overrides,
            });
        }
    }
    for (i, item) in d_array(o.get("multipliers"), &format!("{path}[\"multipliers\"]"))?.into_iter().flatten().enumerate() {
        let p = format!("{path}[\"multipliers\"][{i}]");
        let item = d_object(item, &p)?;
        out.multipliers.push((
            d_record(item.get("when"), &format!("{p}[\"when\"]"))?.clone(),
            d_factor(item.get("factor"), &format!("{p}[\"factor\"]"))?,
        ));
    }
    Ok(Some(out))
}

/// `ConfigProvider.Reasoning`: `{supported: false}` or `{supported: true, toggle?, effort?, budget?}`.
fn d_reasoning(value: Option<&Value>, path: &str) -> Decoded<ConfigReasoning> {
    let o = d_record(value, path)?;
    match o.get("supported") {
        Some(Value::Bool(false)) => Ok(ConfigReasoning::default()),
        Some(Value::Bool(true)) => {
            let budget = match d_opt_record(o.get("budget"), &format!("{path}[\"budget\"]"))? {
                Some(b) => Some(ConfigReasoningBudget {
                    min: d_opt_int(b.get("min"), &format!("{path}[\"budget\"][\"min\"]"))?,
                    max: d_opt_int(b.get("max"), &format!("{path}[\"budget\"][\"max\"]"))?,
                }),
                None => None,
            };
            Ok(ConfigReasoning {
                supported: true,
                toggle: d_opt_bool(o.get("toggle"), &format!("{path}[\"toggle\"]"))?,
                effort: o.get("effort").map(|v| d_strings(Some(v), &format!("{path}[\"effort\"]"))).transpose()?,
                budget,
            })
        }
        other => Err(expected(&format!("{path}[\"supported\"]"), "boolean", other)),
    }
}

#[derive(Debug, Clone)]
struct SourceLimit {
    context: Option<f64>,
    input: Option<f64>,
    output: Option<f64>,
}

#[derive(Debug, Clone)]
struct SourceModel {
    id: String,
    kind: String,
    name: String,
    family: Option<String>,
    release_date: String,
    status: Option<String>,
    input: Vec<String>,
    output: Vec<String>,
    tools: bool,
    reasoning: ConfigReasoning,
    temperature: Option<bool>,
    limit: Option<SourceLimit>,
    cost: Option<SourceCost>,
    /// Ordered: the first API is the model's default.
    api: Object,
    modes: Option<Object>,
}

fn d_model(value: &Value) -> Decoded<SourceModel> {
    let o = d_object(value, "")?;
    let field = |key: &str| format!("[\"{key}\"]");
    let modalities = d_record(o.get("modalities"), &field("modalities"))?;
    let capabilities = d_record(o.get("capabilities"), &field("capabilities"))?;
    let tools = d_record(capabilities.get("tools"), "[\"capabilities\"][\"tools\"]")?;
    let limit = match d_opt_record(o.get("limit"), &field("limit"))? {
        Some(l) => Some(SourceLimit {
            context: d_opt_number(l.get("context"), "[\"limit\"][\"context\"]")?,
            input: d_opt_number(l.get("input"), "[\"limit\"][\"input\"]")?,
            output: d_opt_number(l.get("output"), "[\"limit\"][\"output\"]")?,
        }),
        None => None,
    };
    let modes = match d_opt_record(o.get("experimental"), &field("experimental"))? {
        Some(e) => d_opt_record(e.get("modes"), "[\"experimental\"][\"modes\"]")?.cloned(),
        None => None,
    };
    Ok(SourceModel {
        id: d_string(o.get("id"), &field("id"))?,
        kind: d_string(o.get("type"), &field("type"))?,
        name: d_string(o.get("name"), &field("name"))?,
        family: d_opt_string(o.get("family"), &field("family"))?,
        release_date: d_string(o.get("release_date"), &field("release_date"))?,
        status: d_opt_string(o.get("status"), &field("status"))?,
        input: d_strings(modalities.get("input"), "[\"modalities\"][\"input\"]")?,
        output: d_strings(modalities.get("output"), "[\"modalities\"][\"output\"]")?,
        tools: d_bool(tools.get("supported"), "[\"capabilities\"][\"tools\"][\"supported\"]")?,
        reasoning: d_reasoning(capabilities.get("reasoning"), "[\"capabilities\"][\"reasoning\"]")?,
        temperature: d_opt_bool(capabilities.get("temperature"), "[\"capabilities\"][\"temperature\"]")?,
        limit,
        cost: d_cost(o.get("cost"), &field("cost"))?,
        api: d_record(o.get("api"), &field("api"))?.clone(),
        modes,
    })
}

/// One `api` entry (`SourceApi`); entries this version doesn't understand are skipped.
#[derive(Debug, Clone, Default)]
struct SourceApi {
    base_url: Option<String>,
    aisdk: Option<String>,
    opencodeai: Option<String>,
    reasoning_field: Option<String>,
    max_tokens_field: Option<String>,
    media_input: Option<String>,
}

fn d_api(value: &Value) -> Option<SourceApi> {
    let decode = || -> Decoded<SourceApi> {
        let o = d_object(value, "")?;
        let mut out = SourceApi { base_url: d_opt_string(o.get("base_url"), "")?, ..Default::default() };
        if let Some(sdk) = d_opt_record(o.get("sdk"), "")? {
            out.aisdk = d_opt_string(sdk.get("aisdk"), "")?;
            out.opencodeai = d_opt_string(sdk.get("opencodeai"), "")?;
        }
        if let Some(c) = d_opt_record(o.get("compatibility"), "")? {
            out.reasoning_field = d_opt_string(c.get("reasoning_field"), "")?;
            out.max_tokens_field = d_opt_string(c.get("max_tokens_field"), "")?;
            out.media_input = d_opt_string(c.get("media_input"), "")?;
        }
        Ok(out)
    };
    decode().ok()
}

/// One experimental mode (`SourceMode`).
#[derive(Debug, Clone, Default)]
struct SourceMode {
    service_tier: Option<String>,
    cost: Option<SourceCost>,
    body: Option<Object>,
    headers: Option<HeaderMap>,
}

fn d_mode(value: &Value) -> Option<SourceMode> {
    let decode = || -> Decoded<SourceMode> {
        let o = d_object(value, "")?;
        let headers = match d_opt_record(o.get("headers"), "")? {
            Some(h) => Some(headers_from_value(&Value::Object(h.clone())).ok_or_else(|| "headers".to_string())?),
            None => None,
        };
        Ok(SourceMode {
            service_tier: d_opt_string(o.get("service_tier"), "")?,
            cost: d_cost(o.get("cost"), "")?,
            body: d_opt_record(o.get("body"), "")?.cloned(),
            headers,
        })
    };
    decode().ok()
}

struct SourceProvider<'a> {
    id: String,
    name: String,
    env: Vec<String>,
    models: &'a Object,
}

fn d_provider(value: &Value) -> Option<SourceProvider<'_>> {
    let o = value.as_object()?;
    Some(SourceProvider {
        id: d_string(o.get("id"), "").ok()?,
        name: d_string(o.get("name"), "").ok()?,
        env: d_strings(o.get("env"), "").ok()?,
        models: d_record(o.get("models"), "").ok()?,
    })
}

// ---------------------------------------------------------------------------------------------
// Normalization
// ---------------------------------------------------------------------------------------------

#[derive(Debug, Clone)]
struct Route {
    package: String,
    base_url: Option<String>,
    compatibility: Option<Compatibility>,
}

/// These packages build their URL from provider settings (region, resource, account, project…), which
/// only happens when no baseURL is set. The catalog's URL for them is a template or SDK default.
pub const BUILDS_OWN_URL: &[&str] = &[
    "@opencode/ai/providers/amazon-bedrock",
    "@opencode/ai/providers/azure/chat",
    "@opencode/ai/providers/azure/images",
    "@opencode/ai/providers/azure/responses",
    "@opencode/ai/providers/cloudflare-ai-gateway",
    "@opencode/ai/providers/cloudflare-ai-gateway/decisions",
    "@opencode/ai/providers/cloudflare-workers-ai",
    "@opencode/ai/providers/cloudflare-workers-ai/decisions",
    "@opencode/ai/providers/cloudflare-workers-ai/images",
    "@opencode/ai/providers/google-vertex/gemini",
    "@opencode/ai/providers/google-vertex/images",
    "@opencode/ai/providers/google-vertex/messages",
    "aisdk:@jerome-benoit/sap-ai-provider-v2",
    "aisdk:watsonx-ai-provider",
];

/// Compatible packages are told the real host, so requests and saved provider state carry its ID.
const COMPATIBLE: &[&str] = &["@opencode/ai/providers/openai-compatible", "@opencode/ai/providers/anthropic-compatible"];

/// `loadable`: newer catalogs may name `@opencode/ai` entrypoints that older builds don't ship, so those
/// count as no package.
fn loadable(opencodeai: Option<&str>, unknown: &mut IndexSet<String>) -> Option<String> {
    let opencodeai = opencodeai?;
    let name = package_name(opencodeai);
    if !name.starts_with("@opencode/ai/") || CATALOG_PACKAGES.contains(&name.as_str()) {
        return Some(opencodeai.to_string());
    }
    unknown.insert(name);
    None
}

fn truthy(value: &Option<String>) -> bool {
    value.as_deref().is_some_and(|v| !v.is_empty())
}

/// `route(providerID, model)`: a chat model uses its first API with a package. Other types (image,
/// decision…) only run through the opencode entrypoint for their type (`providers/<P>/images`) that this
/// build can load, and are skipped without one.
fn route(provider_id: &str, model: &SourceModel, unknown: &mut IndexSet<String>) -> Option<Route> {
    let apis: Vec<(String, SourceApi, Option<String>)> = model
        .api
        .iter()
        .filter_map(|(protocol, value)| d_api(value).map(|api| (protocol.clone(), api)))
        .map(|(protocol, api)| {
            let opencodeai = loadable(api.opencodeai.as_deref(), unknown);
            (protocol, api, opencodeai)
        })
        .collect();
    let suffix = format!("/{}s", model.kind);
    let (protocol, api, opencodeai) = if model.kind == "chat" {
        apis.into_iter().find(|(_, api, opencodeai)| truthy(opencodeai) || truthy(&api.aisdk))?
    } else {
        apis.into_iter().find(|(_, _, opencodeai)| opencodeai.as_deref().is_some_and(|o| o.ends_with(&suffix)))?
    };
    let package = match opencodeai {
        Some(package) => package,
        None => {
            let sdk = api.aisdk.clone().filter(|a| !a.is_empty())?;
            let shape = match protocol.as_str() {
                "responses" => Some("responses"),
                "chat-completions" => Some("completions"),
                _ => None,
            };
            let context = NativeContext { provider_id, model_id: Some(&model.id), shape, ..Default::default() };
            native(&sdk, &context).unwrap_or_else(|| aisdk(&sdk))
        }
    };
    if package.is_empty() {
        return None;
    }
    let compatibility = Compatibility {
        reasoning_field: api.reasoning_field.clone().filter(|f| !f.is_empty()),
        max_tokens_field: api
            .max_tokens_field
            .clone()
            .filter(|f| matches!(f.as_str(), "max_completion_tokens" | "max_tokens")),
        media_input: api.media_input.clone().filter(|m| matches!(m.as_str(), "assets" | "state" | "root")),
        ..Default::default()
    };
    let has_compatibility =
        compatibility.reasoning_field.is_some() || compatibility.max_tokens_field.is_some() || compatibility.media_input.is_some();
    // An empty base_url is how models.dev says it has no URL for the API.
    let base_url = if BUILDS_OWN_URL.contains(&package.as_str()) { None } else { api.base_url.clone().filter(|u| !u.is_empty()) };
    Some(Route { package, base_url, compatibility: has_compatibility.then_some(compatibility) })
}

/// `mostCommon(values)`: the most frequent value, the first seen on ties.
fn most_common<T: Clone + Eq + std::hash::Hash>(values: &[T]) -> Option<T> {
    let mut counts: IndexMap<&T, usize> = IndexMap::new();
    for value in values {
        *counts.entry(value).or_insert(0) += 1;
    }
    let mut best: Option<(&T, usize)> = None;
    for (value, count) in counts {
        if best.is_none_or(|(_, n)| count > n) {
            best = Some((value, count));
        }
    }
    best.map(|(value, _)| value.clone())
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

fn zero_cost() -> Cost {
    Cost { tier: None, input: 0.0, output: 0.0, cache: CostCache { read: 0.0, write: 0.0 } }
}

/// `{context: {gt | gte}}` as the only condition: a context tier of that size.
fn context_size(when: &Object) -> Option<f64> {
    if when.len() != 1 {
        return None;
    }
    let context = when.get("context")?.as_object()?;
    let gt = match context.get("gt") {
        None => None,
        Some(v) => Some(v.as_f64()?),
    };
    let gte = match context.get("gte") {
        None => None,
        Some(v) => Some(v.as_f64()?),
    };
    gt.or(gte)
}

/// `cost(input)`: only per-token text prices are kept: per-image, per-second and similar prices have no
/// place in opencode's cost yet. Context overrides become tiers; other conditions (service tier, time of
/// day…) are left out.
fn cost(input: Option<&SourceCost>) -> Vec<Cost> {
    let per_token = |unit: &Option<String>| unit.as_deref().is_none_or(|u| u == "token");
    let (Some(text_input), Some(text_output)) = (input.and_then(|c| c.input.as_ref()), input.and_then(|c| c.output.as_ref())) else {
        return vec![zero_cost()];
    };
    if !per_token(&text_input.unit) || !per_token(&text_output.unit) {
        return vec![zero_cost()];
    }
    let base = Cost {
        tier: None,
        input: text_input.price,
        output: text_output.price,
        cache: CostCache {
            read: text_input.cache.as_ref().and_then(|c| c.read).unwrap_or(0.0),
            write: cache_write(text_input.cache.as_ref()).unwrap_or(0.0),
        },
    };
    // An override merges field by field, so a tier keeps every base price it doesn't set.
    let mut tiers: IndexMap<u64, Cost> = IndexMap::new();
    let tier = |when: &Object, tiers: &mut IndexMap<u64, Cost>| -> Option<u64> {
        let size = context_size(when)?;
        let key = size.to_bits();
        tiers.entry(key).or_insert_with(|| Cost { tier: Some(CostTier::context(size as i64)), ..base.clone() });
        Some(key)
    };
    for o in &text_input.overrides {
        let Some(key) = tier(&o.when, &mut tiers) else { continue };
        let entry = tiers.get_mut(&key).unwrap();
        entry.input = o.price.unwrap_or(entry.input);
        entry.cache = CostCache {
            read: o.cache.as_ref().and_then(|c| c.read).unwrap_or(entry.cache.read),
            write: cache_write(o.cache.as_ref()).unwrap_or(entry.cache.write),
        };
    }
    for o in &text_output.overrides {
        let Some(key) = tier(&o.when, &mut tiers) else { continue };
        let entry = tiers.get_mut(&key).unwrap();
        entry.output = o.price.unwrap_or(entry.output);
    }
    let mut out = vec![base];
    out.extend(tiers.into_values());
    out
}

/// `factorAt(factor, path)`: prices an object factor doesn't name keep factor 1.
fn factor_at(factor: &Factor, path: &[&str]) -> f64 {
    let mut node = Some(factor);
    for key in path {
        node = match node {
            Some(Factor::Map(entries)) => entries.iter().find(|(k, _)| k == key).map(|(_, v)| v),
            other => other,
        };
    }
    match node {
        None => 1.0,
        Some(Factor::Num(n)) => *n,
        // A cache write factor is keyed by TTL; opencode keeps the first one.
        Some(Factor::Map(entries)) => match entries.first() {
            Some((_, Factor::Num(n))) => *n,
            _ => 1.0,
        },
    }
}

/// `Math.round(x)`: rounds half up.
fn js_round(x: f64) -> f64 {
    (x + 0.5).floor()
}

/// `modeCost`: a mode without its own cost takes the base prices scaled by its service tier's multiplier
/// (the mode name by default). The base context tiers are kept unscaled.
fn mode_cost(model: &SourceModel, base: &[Cost], name: &str, mode: &SourceMode) -> Vec<Cost> {
    let tiers = base.iter().skip(1).cloned();
    if let Some(cost_) = &mode.cost {
        return std::iter::once(cost(Some(cost_)).remove(0)).chain(tiers).collect();
    }
    let tier = mode.service_tier.as_deref().unwrap_or(name);
    let factor = model.cost.as_ref().and_then(|c| {
        c.multipliers.iter().find(|(when, _)| {
            when.len() == 1
                && when.get("service_tier").and_then(Value::as_object).is_some_and(|condition| match condition.get("equals") {
                    Some(Value::String(equals)) => equals == tier,
                    _ => false,
                })
        })
    });
    let (Some((_, factor)), Some(default)) = (factor, base.first()) else { return base.to_vec() };
    let scale = |price: f64, path: &[&str]| js_round(price * factor_at(factor, path) * 1e10) / 1e10;
    let scaled = Cost {
        tier: None,
        input: scale(default.input, &["text", "input", "price"]),
        output: scale(default.output, &["text", "output", "price"]),
        cache: CostCache {
            read: scale(default.cache.read, &["text", "input", "cache", "read"]),
            write: scale(default.cache.write, &["text", "input", "cache", "write"]),
        },
    };
    std::iter::once(scaled).chain(tiers).collect()
}

/// v2 lists exact MIME types; opencode's capabilities use the broad kind.
fn modality(value: &str) -> String {
    if value == "application/pdf" {
        return "pdf".into();
    }
    let kind = value.split('/').next().unwrap_or("");
    if matches!(kind, "text" | "image" | "audio" | "video") { kind.into() } else { value.into() }
}

fn modalities(values: &[String]) -> Vec<String> {
    let mut out: Vec<String> = vec![];
    for value in values.iter().map(|v| modality(v)) {
        if !out.contains(&value) {
            out.push(value);
        }
    }
    out
}

struct ProviderDefaults<'a> {
    package: &'a str,
    base_url: Option<&'a str>,
}

#[derive(Default)]
struct ModelInput {
    name: Option<String>,
    cost: Option<Vec<Cost>>,
    headers: Option<HeaderMap>,
    body: Option<Object>,
    variants: Option<Vec<Variant>>,
}

fn model_info(provider_id: &str, provider: &ProviderDefaults<'_>, id: String, model: &SourceModel, route: &Route, input: ModelInput) -> ModelInfo {
    // Per model, so it never merges into a model that overrides to a different package.
    let mut settings = Object::new();
    if let Some(url) = &route.base_url
        && Some(url.as_str()) != provider.base_url
    {
        settings.insert("baseURL".into(), json!(url));
    }
    if COMPATIBLE.contains(&route.package.as_str()) {
        settings.insert("provider".into(), json!(provider_id));
    }
    let limit = model.limit.as_ref();
    ModelInfo {
        id,
        model_id: model.id.clone(),
        provider_id: provider_id.to_string(),
        canonical: None,
        kind: Some(model.kind.clone()),
        family: model.family.clone().filter(|f| !f.is_empty()),
        name: input.name.unwrap_or_else(|| model.name.clone()),
        compatibility: route.compatibility.clone(),
        package: (route.package != provider.package).then(|| route.package.clone()),
        settings: if settings.is_empty() { None } else { Some(settings) },
        headers: input.headers,
        body: input.body,
        capabilities: Capabilities {
            tools: model.tools,
            input: modalities(&model.input),
            output: modalities(&model.output),
            temperature: model.temperature,
        },
        variants: input.variants.unwrap_or_default(),
        time: ModelTime { released: released(&model.release_date) },
        cost: input.cost.unwrap_or_else(|| cost(model.cost.as_ref())),
        status: match model.status.as_deref() {
            Some("alpha") => ModelStatus::Alpha,
            Some("beta") => ModelStatus::Beta,
            Some("deprecated") => ModelStatus::Deprecated,
            _ => ModelStatus::Active,
        },
        enabled: true,
        limit: Limit {
            context: limit.and_then(|l| l.context).unwrap_or(0.0) as i64,
            input: limit.and_then(|l| l.input).map(|v| v as i64),
            output: limit.and_then(|l| l.output).unwrap_or(0.0) as i64,
        },
    }
}

/// `normalize(catalog)`: models.dev v2 (or v1, converted) providers → catalog snapshots.
pub fn normalize(input: &Object) -> Vec<Snapshot> {
    let converted;
    let catalog = if is_v1(input) {
        converted = to_v2(input);
        &converted
    } else {
        input
    };
    let mut failed: Vec<(String, String)> = vec![];
    let mut unknown: IndexSet<String> = IndexSet::new();
    let mut snapshots = vec![];
    for value in catalog.values() {
        let Some(item) = d_provider(value) else { continue };
        let provider_id = item.id.clone();
        let mut sources: Vec<(SourceModel, Route)> = vec![];
        for (key, value) in item.models {
            let model = match d_model(value) {
                Ok(model) => model,
                Err(issue) => {
                    failed.push((format!("{}/{key}", item.id), issue));
                    continue;
                }
            };
            // The fetch already asks for these types, but custom files and v1 catalogs may hold any type.
            if !TYPES.contains(&model.kind.as_str()) {
                continue;
            }
            if let Some(target) = route(&provider_id, &model, &mut unknown) {
                sources.push((model, target));
            }
        }
        if sources.is_empty() {
            continue;
        }
        // v2 lists APIs per model, so the provider defaults are what most of its models use. A model that
        // builds its own URL would inherit a provider baseURL, so then every model carries its own instead.
        let packages: Vec<String> = sources.iter().map(|(_, r)| r.package.clone()).collect();
        let Some(package) = most_common(&packages) else { continue };
        let urls: Vec<Option<String>> = sources.iter().map(|(_, r)| r.base_url.clone()).collect();
        let base_url = if urls.iter().all(Option::is_some) { most_common(&urls).flatten() } else { None };
        let info = ProviderInfo {
            id: provider_id.clone(),
            canonical: None,
            integration_id: None,
            name: item.name.clone(),
            activation: Activation::Auto,
            package: package.clone(),
            compatibility: None,
            settings: base_url.as_ref().map(|url| {
                let mut o = Object::new();
                o.insert("baseURL".into(), json!(url));
                o
            }),
            headers: None,
            body: None,
        };
        let defaults = ProviderDefaults { package: &package, base_url: base_url.as_deref() };
        let mut models = vec![];
        for (model, route) in &sources {
            let base_cost = cost(model.cost.as_ref());
            let mut base = model_info(
                &provider_id,
                &defaults,
                model.id.clone(),
                model,
                route,
                ModelInput { cost: Some(base_cost.clone()), ..Default::default() },
            );
            let mut probe = base.clone();
            probe.package = Some(route.package.clone());
            let variants = variant::resolve(&probe, &variant::supports(&model.reasoning));
            base.variants = variants.clone();
            models.push(base);
            for (mode, options) in model.modes.iter().flatten().filter_map(|(k, v)| d_mode(v).map(|m| (k, m))) {
                let mut chars = mode.chars();
                let capitalized = match chars.next() {
                    Some(c) => c.to_uppercase().collect::<String>() + chars.as_str(),
                    None => String::new(),
                };
                models.push(model_info(
                    &provider_id,
                    &defaults,
                    format!("{}-{mode}", model.id),
                    model,
                    route,
                    ModelInput {
                        name: Some(format!("{} {capitalized}", model.name)),
                        cost: Some(mode_cost(model, &base_cost, mode, &options)),
                        headers: options.headers.clone(),
                        body: options.body.clone(),
                        variants: Some(variants.clone()),
                    },
                ));
            }
        }
        snapshots.push(Snapshot { info, models, environment: item.env.clone() });
    }
    if !failed.is_empty() {
        let models: Vec<&str> = failed.iter().take(10).map(|(m, _)| m.as_str()).collect();
        tracing::warn!(count = failed.len(), models = ?models, issue = %failed[0].1, "Skipped models.dev models that failed to parse");
    }
    if !unknown.is_empty() {
        tracing::debug!(entrypoints = ?unknown, "Skipped models.dev APIs with entrypoints this build doesn't have");
    }
    snapshots
}

/// Decode `api.json` text as a JSON record.
pub fn decode_catalog(text: &str) -> Result<Object, String> {
    match serde_json::from_str::<Value>(text) {
        Ok(Value::Object(o)) => Ok(o),
        Ok(_) => Err("Expected an object".into()),
        Err(e) => Err(e.to_string()),
    }
}

/// `decodeSnapshot(text)`: decode and normalize a catalog text.
pub fn decode_snapshot(text: &str) -> Result<Vec<Snapshot>, String> {
    decode_catalog(text).map(|catalog| normalize(&catalog))
}

/// Bedrock entries that require inference profiles on Bedrock Runtime.
pub const BEDROCK_PROFILE_ONLY_IDS: &[&str] = &[
    "amazon.nova-2-lite-v1:0",
    "anthropic.claude-fable-5",
    "anthropic.claude-fable-5-1",
    "anthropic.claude-haiku-4-5-20251001-v1:0",
    "anthropic.claude-haiku-5-5",
    "anthropic.claude-opus-4-1-20250805-v1:0",
    "anthropic.claude-opus-4-5-20251101-v1:0",
    "anthropic.claude-opus-4-7",
    "anthropic.claude-opus-4-8",
    "anthropic.claude-opus-5",
    "anthropic.claude-sonnet-4-5-20250929-v1:0",
    "anthropic.claude-sonnet-5",
    "anthropic.claude-sonnet-5-5",
    "deepseek.r1-v1:0",
    "meta.llama4-maverick-17b-instruct-v1:0",
    "meta.llama4-scout-17b-instruct-v1:0",
    "mistral.pixtral-large-2502-v1:0",
    "writer.palmyra-x4-v1:0",
    "writer.palmyra-x5-v1:0",
];

/// Providers replaced by canonical Azure / Google Vertex.
pub const DROPPED_PROVIDERS: &[&str] = &["azure-cognitive-services", "google-vertex-anthropic"];

/// `snapshots(data)` of the models.dev plugin: drop legacy aliases, deprecated and Bedrock profile-only
/// models, and chat models without text. Non-chat models are kept for lookups; the model service never
/// offers them for sessions.
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
                        && (m.kind.as_deref().unwrap_or("chat") != "chat" || supports_text(&m.capabilities))
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
    /// The `ETag` response header.
    pub etag: Option<String>,
}

/// `GET url` with a `User-Agent` header and, when given, `If-None-Match` (one attempt; retries are
/// applied by [`ModelsDev`]).
#[async_trait]
pub trait HttpGet: Send + Sync {
    async fn get(&self, url: &str, user_agent: &str, if_none_match: Option<&str>) -> Result<HttpResponse, String>;
}

/// reqwest-backed [`HttpGet`].
pub struct ReqwestGet(pub reqwest::Client);

impl Default for ReqwestGet {
    fn default() -> Self {
        Self(oc_util::http::client())
    }
}

#[async_trait]
impl HttpGet for ReqwestGet {
    async fn get(&self, url: &str, user_agent: &str, if_none_match: Option<&str>) -> Result<HttpResponse, String> {
        let mut request = self.0.get(url).header("User-Agent", user_agent);
        if let Some(etag) = if_none_match {
            request = request.header("If-None-Match", etag);
        }
        for (k, v) in oc_util::trace::headers() {
            request = request.header(k, v);
        }
        let response = request.send().await.map_err(|e| e.to_string())?;
        let status = response.status().as_u16();
        let etag = response.headers().get("etag").and_then(|v| v.to_str().ok()).map(String::from);
        let body = response.text().await.map_err(|e| e.to_string())?;
        Ok(HttpResponse { status, body, etag })
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
}

/// The stored check state (`CacheMeta`): when the catalog was last checked, its digest and `ETag`.
struct Meta {
    updated_at: f64,
    digest: Option<String>,
    etag: Option<String>,
}

fn decode_meta(value: &Value) -> Option<Meta> {
    let o = value.as_object()?;
    let text = |key: &str| match o.get(key) {
        None => Some(None),
        Some(Value::String(s)) => Some(Some(s.clone())),
        Some(_) => None,
    };
    Some(Meta { updated_at: o.get("updatedAt")?.as_f64()?, digest: text("digest")?, etag: text("etag")? })
}

fn now_ms() -> f64 {
    chrono::Utc::now().timestamp_millis() as f64
}

/// Transient statuses retried by `HttpClient.retryTransient`.
fn transient(status: u16) -> bool {
    matches!(status, 408 | 429 | 500 | 502 | 503 | 504)
}

/// A failed fetch: the non-2xx status when the server answered.
struct FetchError {
    status: Option<u16>,
    message: String,
}

impl From<FetchError> for ModelsDevError {
    fn from(error: FetchError) -> Self {
        ModelsDevError(error.message)
    }
}

/// The models.dev service: memoized catalog, cache, refresh.
pub struct ModelsDev {
    options: ModelsDevOptions,
    cache: Arc<dyn CatalogCache>,
    http: Arc<dyn HttpGet>,
    key: String,
    meta_key: String,
    memo: parking_lot::Mutex<Option<Arc<Vec<Snapshot>>>>,
    populating: tokio::sync::Mutex<()>,
    lock: tokio::sync::Mutex<()>,
    /// The startup cleanup of the v1 cache rows (done once, before the first read).
    started: tokio::sync::OnceCell<()>,
    events: tokio::sync::broadcast::Sender<()>,
    /// Set by [`ModelsDev::refresh_on_first_use`]: the service starts its refresh loop when it is first used.
    lazy_refresh: parking_lot::Mutex<Option<std::sync::Weak<ModelsDev>>>,
}

impl ModelsDev {
    pub fn new(options: ModelsDevOptions, cache: Arc<dyn CatalogCache>, http: Arc<dyn HttpGet>) -> Self {
        let key = cache_key(options.source());
        let meta_key = format!("{key}:meta");
        let (events, _) = tokio::sync::broadcast::channel(16);
        Self {
            options,
            cache,
            http,
            key,
            meta_key,
            memo: parking_lot::Mutex::new(None),
            populating: tokio::sync::Mutex::new(()),
            lock: tokio::sync::Mutex::new(()),
            started: tokio::sync::OnceCell::new(),
            events,
            lazy_refresh: parking_lot::Mutex::new(None),
        }
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

    /// The v1 catalog was cached under unversioned keys; free that multi-MB row (done when the service
    /// is built upstream, here before its first use).
    async fn start(&self) {
        self.started
            .get_or_init(|| async {
                let legacy = legacy_cache_key(&self.key);
                self.cache.remove(&legacy).await;
                self.cache.remove(&format!("{legacy}:meta")).await;
            })
            .await;
    }

    async fn load_from_cache(&self) -> Option<Cached> {
        let value = self.cache.get(&self.key).await?;
        let decoded = (|| {
            let o = value.as_object()?;
            o.get("updatedAt")?.as_f64()?;
            match o.get("digest") {
                None | Some(Value::String(_)) => {}
                Some(_) => return None,
            }
            let catalog = decode_catalog(o.get("body")?.as_str()?).ok()?;
            Some(Cached { catalog })
        })();
        if decoded.is_none() {
            self.cache.remove(&self.key).await;
            self.cache.remove(&self.meta_key).await;
        }
        decoded
    }

    /// `GET <source><path>`: 2 retries on transport errors / transient statuses (jittered exponential
    /// backoff from 200 ms), non-2xx fails, everything within 10 s.
    async fn fetch_path(&self, path: &str, etag: Option<&str>) -> Result<HttpResponse, FetchError> {
        let url = format!("{}{path}", self.options.source());
        let attempt = async {
            let mut delay = 200.0f64;
            let mut retries = 0;
            loop {
                let result = self.http.get(&url, &self.options.user_agent, etag).await;
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
                let response = result.map_err(|message| FetchError { status: None, message })?;
                if !(200..300).contains(&response.status) {
                    return Err(FetchError {
                        status: Some(response.status),
                        message: format!("StatusCode: non 2xx status code ({} GET {url})", response.status),
                    });
                }
                return Ok(response);
            }
        };
        tokio::time::timeout(FETCH_TIMEOUT, attempt)
            .await
            .map_err(|_| FetchError { status: None, message: "Request timed out".into() })?
    }

    /// `fetchApi(etag)`: the v2 catalog; a custom mirror that has none (404) may serve the v1 catalog,
    /// which is converted when decoded.
    async fn fetch_api(&self, etag: Option<&str>) -> Result<HttpResponse, FetchError> {
        match self.fetch_path(CATALOG_PATH, etag).await {
            Err(FetchError { status: Some(404), .. }) if self.options.source() != DEFAULT_SOURCE => {
                self.fetch_path("/api.json", etag).await
            }
            other => other,
        }
    }

    async fn write_meta(&self, updated_at: f64, digest: Option<&str>, etag: Option<&str>) -> Result<(), String> {
        let mut value = Object::new();
        value.insert("updatedAt".into(), crate::json::num(updated_at));
        if let Some(digest) = digest.filter(|d| !d.is_empty()) {
            value.insert("digest".into(), json!(digest));
        }
        if let Some(etag) = etag.filter(|e| !e.is_empty()) {
            value.insert("etag".into(), json!(etag));
        }
        self.cache.set(&self.meta_key, Value::Object(value)).await
    }

    /// Best-effort: a cache-write failure never fails the catalog.
    async fn write_cache(&self, text: &str, etag: Option<&str>, digest: Option<String>) {
        let digest = digest.unwrap_or_else(|| body_digest(text));
        let updated_at = now_ms();
        let value = json!({"updatedAt": crate::json::num(updated_at), "digest": digest, "body": text});
        let result = match self.cache.set(&self.key, value).await {
            Ok(()) => self.write_meta(updated_at, Some(&digest), etag).await,
            Err(cause) => Err(cause),
        };
        if let Err(cause) = result {
            tracing::warn!(cause = %cause, "Failed to cache models.dev catalog");
        }
    }

    async fn fetch_and_write(&self) -> Result<Object, ModelsDevError> {
        let response = self.fetch_api(None).await?;
        let catalog = decode_catalog(&response.body).map_err(ModelsDevError)?;
        self.write_cache(&response.body, response.etag.as_deref(), None).await;
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
        self.start().await;
        self.start_pending_refresh();
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

    /// `refresh(force)`: skip a fresh check (< 5 min) unless forced, revalidate with the stored `ETag`,
    /// and treat a 304 or a byte-identical body as unchanged (only the check time is recorded). Errors
    /// are logged (`"Failed to fetch models.dev"`) and ignored. Returns whether the catalog changed.
    pub async fn refresh(&self, force: bool) -> bool {
        self.start().await;
        let _guard = self.lock.lock().await;
        let result: Result<bool, ModelsDevError> = async {
            let stored = match self.cache.get(&self.meta_key).await {
                Some(meta) => decode_meta(&meta),
                None => self.cache.get(&self.key).await.as_ref().and_then(decode_meta),
            };
            if !force
                && let Some(stored) = &stored
                && now_ms() - stored.updated_at < TTL.as_millis() as f64
            {
                return Ok(false);
            }
            let etag = if force { None } else { stored.as_ref().and_then(|s| s.etag.as_deref()) };
            let response = match self.fetch_api(etag).await {
                Ok(response) => Some(response),
                Err(FetchError { status: Some(304), .. }) => None,
                Err(error) => return Err(error.into()),
            };
            let digest = response.as_ref().map(|r| body_digest(&r.body));
            // models.dev rarely changes between polls; skip invalidation and the Refreshed event for an
            // unchanged catalog so downstream provider/model update listeners stay quiet.
            if !force
                && let Some(stored) = &stored
                && (response.is_none() || stored.digest == digest)
            {
                let etag = response.as_ref().and_then(|r| r.etag.clone()).or_else(|| stored.etag.clone());
                self.write_meta(now_ms(), stored.digest.as_deref(), etag.as_deref()).await.map_err(ModelsDevError)?;
                return Ok(false);
            }
            let Some(response) = response else { return Ok(false) };
            decode_catalog(&response.body).map_err(ModelsDevError)?;
            self.write_cache(&response.body, response.etag.as_deref(), digest).await;
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

    /// Start the refresh loop when the service is first used, like upstream's lazily built `ModelsDev`
    /// layer, which forks its refresh when the first location needs the catalog.
    pub fn refresh_on_first_use(self: &Arc<Self>) {
        *self.lazy_refresh.lock() = Some(Arc::downgrade(self));
    }

    /// Start the refresh loop set up by [`ModelsDev::refresh_on_first_use`], if it has not started yet.
    pub fn start_pending_refresh(&self) {
        let lazy = self.lazy_refresh.lock().take();
        if let Some(this) = lazy.and_then(|w| w.upgrade()) {
            this.spawn_refresh_loop();
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
    // The models.dev plugin registers integrations from its filtered snapshots, so the deprecated alias
    // providers never become integrations (core/src/plugin/models-dev.ts `snapshots`).
    data.iter()
        .filter(|p| !p.environment.is_empty() && !DROPPED_PROVIDERS.contains(&p.info.id.as_str()))
        .map(|p| IntegrationSpec {
            id: p.info.id.clone(),
            name: Some(p.info.name.clone()),
            methods: vec![IntegrationMethod::Key { label: None }, IntegrationMethod::Env { names: p.environment.clone() }],
        })
        .collect()
}
