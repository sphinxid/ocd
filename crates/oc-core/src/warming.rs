//! The built-in `opencode.warming` plugin (`core/src/plugin/warming.ts`, 2.3.1): opt-in keep-alive requests that
//! stop a top-level session's provider prompt cache from expiring while the user pauses between prompts.
//!
//! After a non-warming request (a step's `context` hook, a compaction's `compaction` hook, or another session
//! generation's `generate` hook), the session gets a loop that sends the keep-alive prompt through
//! [`crate::runner::Engine::session_generate`] every `interval` until `duration` has passed since that request. A
//! model's or provider's `cache.warming` overrides the top-level `warming`; `interval: "auto"` pings at 90% of the
//! model's `cache.ttl` (4 minutes without one). The hooks run in [`crate::plugin_request::shape`]; request ends
//! and deletions arrive as bus events ([`Warming::event`]).

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::Duration;

use futures::future::BoxFuture;
use oc_config::schema::{Warming as Configured, WarmingInterval};
use parking_lot::Mutex;
use serde_json::Value;
use tokio::sync::watch;
use tokio::time::Instant;

/// The plugin id (gated by the location's `plugins` configuration).
pub const PLUGIN_ID: &str = "opencode.warming";

/// `defaults.prompt`.
pub const DEFAULT_PROMPT: &str = oc_config::fold::WARMING_PROMPT;

/// `defaults.duration`: 30 minutes.
const DEFAULT_DURATION_MS: f64 = 30.0 * 60_000.0;

/// `fallbackInterval`: the interval for `"auto"` when the model has no `cache.ttl`, a little under the five minutes
/// most explicit caches last.
const FALLBACK_INTERVAL_MS: f64 = 4.0 * 60_000.0;

/// `fallbackTTL`.
const FALLBACK_TTL_MS: f64 = 5.0 * 60_000.0;

/// The bus events that end a running request (and `session.deleted`, which also stops the session's loop). A turn
/// always ends with an execution event and a compaction with its own, so a request that ends without its own event
/// can't stay running past them.
pub const ENDS: [&str; 8] = [
    "session.step.streamed",
    "session.step.failed",
    "session.compaction.ended",
    "session.compaction.failed",
    "session.execution.succeeded",
    "session.execution.failed",
    "session.execution.interrupted",
    "session.deleted",
];

/// Cache usage a ping reported (`result.tokens.cache`).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct CacheUsage {
    pub read: u64,
    pub write: u64,
}

/// Sends a keep-alive request: `ctx.session.generate({ sessionID, prompt })`.
pub trait Generator: Send + Sync + 'static {
    fn generate(&self, session_id: String, prompt: String) -> BoxFuture<'static, anyhow::Result<CacheUsage>>;
}

/// The settings of a warmed session (`Settings`), durations in milliseconds.
#[derive(Debug, Clone, PartialEq)]
pub struct Settings {
    pub prompt: String,
    pub interval: f64,
    pub duration: f64,
    /// How long the provider keeps a cache entry after the request that last sent it.
    pub ttl: f64,
}

/// Whether a session is warmed after a request, and why not (`Decision`).
#[derive(Debug, Clone, PartialEq)]
pub enum Decision {
    Warm(Settings),
    /// Neither the top-level `warming` nor the model's `cache.warming` turns it on.
    Off,
    /// `"auto"` timing on a cache that lasts longer than `duration`, so no ping is needed before the window closes.
    CacheOutlivesWindow,
    /// An explicit interval that isn't positive and shorter than a finite `duration`.
    InvalidInterval { interval: f64, duration: f64 },
}

/// `parseTTL`: a `cache.ttl` such as "5m" or "1h" in milliseconds.
pub fn parse_ttl(ttl: Option<&str>) -> Option<f64> {
    let ttl = ttl?;
    let digits = ttl.bytes().take_while(|b| b.is_ascii_digit() || *b == b'.').count();
    if digits == 0 {
        return None;
    }
    let unit = match &ttl[digits..] {
        "ms" => 1.0,
        "s" => 1_000.0,
        "m" => 60_000.0,
        "h" => 3_600_000.0,
        "d" => 86_400_000.0,
        _ => return None,
    };
    // `Number("1.2.3")` is NaN, which `Duration.millis` turns into zero.
    let value = ttl[..digits].parse::<f64>().unwrap_or(0.0);
    Some(value * unit)
}

/// `WarmingPlugin.decide`: `cache` is the request model's `settings.cache` (provider settings overlaid by the
/// model's), `global` the latest top-level `warming`.
pub fn decide(cache: Option<&serde_json::Map<String, Value>>, global: Option<&Configured>, model: &str) -> Decision {
    let raw = cache.and_then(|c| c.get("warming"));
    let local = raw.and_then(<Configured as oc_config::decode::Decode>::decode);
    if raw.is_some() && local.is_none() {
        tracing::warn!(model, "invalid cache.warming setting; using the top-level warming setting");
    }
    // `local ?? global`: a model's `false` turns warming off whatever the top-level says.
    let on = match local.as_ref().or(global) {
        None | Some(Configured::Enabled(false)) => false,
        Some(_) => true,
    };
    if !on {
        return Decision::Off;
    }
    // The keep-alive prompt is only set at the top level; a model's setting changes timing.
    let mut prompt = DEFAULT_PROMPT.to_string();
    let mut interval = WarmingInterval::Auto;
    let mut duration = DEFAULT_DURATION_MS;
    for layer in [global, local.as_ref()].into_iter().flatten() {
        let Configured::Settings(info) = layer else { continue };
        if let Some(p) = &info.prompt {
            prompt = p.clone();
        }
        if let Some(i) = &info.interval {
            interval = i.clone();
        }
        if let Some(d) = &info.duration {
            duration = d.to_millis();
        }
    }
    let ttl = parse_ttl(cache.and_then(|c| c.get("ttl")).and_then(Value::as_str));
    let auto = interval == WarmingInterval::Auto;
    // "auto" pings at 90% of the cache's lifetime, so it lands before the cache expires.
    let interval = match &interval {
        WarmingInterval::Auto => ttl.map_or(FALLBACK_INTERVAL_MS, |t| t * 0.9),
        WarmingInterval::Duration(d) => d.to_millis(),
    };
    // A cache that outlives the window, such as "1h" with the 30-minute default, needs no keep-alive.
    if auto && duration.is_finite() && interval >= duration {
        return Decision::CacheOutlivesWindow;
    }
    if duration.is_finite() && interval > 0.0 && interval < duration {
        // Without a cache.ttl, an explicit interval longer than five minutes implies a cache that lasts at least that long.
        return Decision::Warm(Settings { prompt, interval, duration, ttl: ttl.unwrap_or(FALLBACK_TTL_MS.max(interval)) });
    }
    Decision::InvalidInterval { interval, duration }
}

/// `text(event)`: the request's last message when it is a user message of exactly one text part.
pub fn last_text(messages: &[oc_ai::types::Message]) -> Option<&str> {
    let message = messages.last()?;
    if message.role != oc_ai::types::Role::User || message.content.len() != 1 {
        return None;
    }
    match &message.content[0] {
        oc_ai::types::ContentPart::Text(part) => Some(part.text.as_str()),
        _ => None,
    }
}

/// A request's hook call.
#[derive(Debug, Clone)]
pub struct Hook {
    /// The location the request runs in (one plugin instance per location).
    pub key: String,
    pub session_id: String,
    /// Whether the request reports its end (`context` and `compaction`; a `generate` request reports none).
    pub tracked: bool,
    /// The session is a subagent's (it has a parent): never warmed.
    pub child: bool,
    /// The request model's `settings.cache`.
    pub cache: Option<serde_json::Map<String, Value>>,
    /// The location's latest top-level `warming`.
    pub global: Option<Configured>,
    /// `providerID/modelID` for the logs.
    pub model: String,
    /// [`last_text`] of the request.
    pub text: Option<String>,
}

/// One session's loop: its handle and which run it is.
struct Loop {
    run: u64,
    handle: tokio::task::AbortHandle,
}

/// One location's plugin instance.
#[derive(Default)]
struct Instance {
    /// Each warmed session's loop. A real request replaces it, and closing the plugin aborts them all.
    loops: HashMap<String, Loop>,
    /// A request runs from its context or compaction hook until an end event completes and removes its entry.
    running: HashMap<String, watch::Sender<bool>>,
    /// Sessions already told their cache outlives the window; the check runs on every request.
    outlived: HashSet<String>,
}

/// The warming plugin's state across locations.
#[derive(Default)]
pub struct Warming {
    generator: parking_lot::RwLock<Option<Arc<dyn Generator>>>,
    instances: Mutex<HashMap<String, Instance>>,
    runs: std::sync::atomic::AtomicU64,
}

impl Warming {
    pub fn new() -> Arc<Warming> {
        Arc::new(Warming::default())
    }

    /// Sets how pings are sent (the session runtime).
    pub fn set_generator(&self, generator: Arc<dyn Generator>) {
        *self.generator.write() = Some(generator);
    }

    /// Feeds the bus to [`Warming::event`] until the bus closes or `self` is dropped.
    pub fn listen(self: &Arc<Self>, mut events: tokio::sync::broadcast::Receiver<crate::bus::Event>) {
        let me = Arc::downgrade(self);
        tokio::spawn(async move {
            loop {
                let event = match events.recv().await {
                    Ok(event) => event,
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                    Err(_) => return,
                };
                if !ENDS.contains(&event.kind.as_str()) {
                    continue;
                }
                let Some(warming) = me.upgrade() else { return };
                let session_id = event.data.get("sessionID").and_then(Value::as_str).unwrap_or_default();
                warming.event(&event.kind, session_id);
            }
        });
    }

    /// An end event: the session's running request (if any) ended; `session.deleted` also stops its loop.
    pub fn event(&self, kind: &str, session_id: &str) {
        let mut instances = self.instances.lock();
        for instance in instances.values_mut() {
            if let Some(request) = instance.running.remove(session_id) {
                let _ = request.send(true);
            }
            if kind != "session.deleted" {
                continue;
            }
            instance.outlived.remove(session_id);
            if let Some(stopped) = instance.loops.remove(session_id) {
                stopped.handle.abort();
            }
        }
    }

    /// Closes the location's plugin instance (the location closed, or the plugin restarted or was disabled):
    /// every loop of it stops.
    pub fn close(&self, key: &str) {
        let Some(instance) = self.instances.lock().remove(key) else { return };
        for stopped in instance.loops.into_values() {
            stopped.handle.abort();
        }
    }

    /// Whether the session has a warming loop.
    pub fn warming(&self, session_id: &str) -> bool {
        self.instances.lock().values().any(|i| i.loops.contains_key(session_id))
    }

    /// The `context` / `compaction` / `generate` hook.
    pub fn hook(self: &Arc<Self>, hook: Hook) {
        if hook.child {
            return;
        }
        let decision = decide(hook.cache.as_ref(), hook.global.as_ref(), &hook.model);
        let mut instances = self.instances.lock();
        let instance = instances.entry(hook.key.clone()).or_default();
        if hook.tracked && !instance.running.contains_key(&hook.session_id) {
            instance.running.insert(hook.session_id.clone(), watch::channel(false).0);
        }
        let stop = |instance: &mut Instance| {
            if let Some(stopped) = instance.loops.remove(&hook.session_id) {
                stopped.handle.abort();
            }
        };
        match decision {
            Decision::Warm(settings) => {
                // Our own ping runs these hooks too.
                if hook.text.as_deref() == Some(settings.prompt.as_str()) {
                    return;
                }
                instance.outlived.remove(&hook.session_id);
                if !instance.loops.contains_key(&hook.session_id) {
                    tracing::info!(sessionID = %hook.session_id, interval = settings.interval, "scheduled session warming");
                }
                // Replacing the loop restarts the interval and the window, and cancels a ping in flight.
                stop(instance);
                let run = self.runs.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                let me = Arc::downgrade(self);
                let (key, session_id) = (hook.key.clone(), hook.session_id.clone());
                let sent = Instant::now();
                // The loop is forked from the request's hook, so its pings stay in that request's trace.
                let handle = tokio::spawn(oc_util::trace::inherit(async move {
                    warm(me.clone(), &key, &session_id, settings, sent).await;
                    // A loop that ended by itself leaves the map, like a fiber that completes leaves its FiberMap.
                    if let Some(warming) = me.upgrade()
                        && let Some(instance) = warming.instances.lock().get_mut(&key)
                        && instance.loops.get(&session_id).is_some_and(|l| l.run == run)
                    {
                        instance.loops.remove(&session_id);
                    }
                }))
                .abort_handle();
                instance.loops.insert(hook.session_id.clone(), Loop { run, handle });
            }
            Decision::Off => stop(instance),
            Decision::InvalidInterval { interval, duration } => {
                tracing::warn!(interval, duration, "warming interval must be positive and shorter than a finite duration");
                stop(instance);
            }
            Decision::CacheOutlivesWindow => {
                stop(instance);
                if instance.outlived.insert(hook.session_id.clone()) {
                    tracing::info!(sessionID = %hook.session_id, model = %hook.model, "not warming: the prompt cache outlives the warming window");
                }
            }
        }
    }

    /// The session's running request, if one is running.
    fn running(&self, key: &str, session_id: &str) -> Option<watch::Receiver<bool>> {
        self.instances.lock().get(key)?.running.get(session_id).map(watch::Sender::subscribe)
    }
}

/// Pings through the session runtime (`ctx.session.generate`): the session's context plus the prompt, usage
/// recorded with source `generate`, history untouched.
pub struct EngineGenerator(pub std::sync::Weak<crate::runner::Engine>);

impl Generator for EngineGenerator {
    fn generate(&self, session_id: String, prompt: String) -> BoxFuture<'static, anyhow::Result<CacheUsage>> {
        let engine = self.0.clone();
        Box::pin(async move {
            let Some(engine) = engine.upgrade() else { anyhow::bail!("the session runtime stopped") };
            let result = engine.session_generate(&session_id, &prompt).await?;
            Ok(CacheUsage { read: result.tokens.cache.read, write: result.tokens.cache.write })
        })
    }
}

/// A millisecond count as a `Duration`, capped at about 30 years so a far-off window can't overflow an `Instant`.
fn millis(ms: f64) -> Duration {
    Duration::from_secs_f64(ms.clamp(0.0, 1e12) / 1000.0)
}

/// `WarmingPlugin.warm`: the interval counts from when the latest request or ping was sent, since that is when the
/// cache's lifetime starts.
async fn warm(warming: std::sync::Weak<Warming>, key: &str, session_id: &str, settings: Settings, mut sent: Instant) {
    let expires = sent + millis(settings.duration);
    loop {
        let due = (sent + millis(settings.interval)).min(expires);
        tokio::time::sleep_until(due).await;
        if Instant::now() >= expires {
            return;
        }
        let Some(me) = warming.upgrade() else { return };
        if let Some(mut request) = me.running(key, session_id) {
            // Ping as soon as the request ends, unless it outlasted the cache: then a ping would only write a new
            // one, so the next real request starts warming again.
            let lasts = (sent + millis(settings.ttl)).saturating_duration_since(Instant::now());
            match tokio::time::timeout(lasts, request.wait_for(|ended| *ended)).await {
                Ok(Ok(_)) => {}
                _ => return,
            }
        }
        sent = Instant::now();
        tracing::info!(sessionID = session_id, "warming session");
        let Some(generator) = me.generator.read().clone() else { return };
        drop(me);
        match generator.generate(session_id.to_string(), settings.prompt.clone()).await {
            Err(cause) => tracing::warn!(sessionID = session_id, cause = %cause, "failed to warm session"),
            // A ping that wrote more than it read rebuilt the cache instead of keeping it, so later pings would keep
            // paying for that. Providers that report no cache usage never count as a miss.
            Ok(usage) if usage.read < usage.write => {
                tracing::info!(sessionID = session_id, read = usage.read, write = usage.write, "stopped warming session after a cache miss");
                return;
            }
            Ok(_) => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use oc_config::schema::{Duration as ConfigDuration, WarmingInfo};
    use serde_json::json;

    fn cache(value: Value) -> serde_json::Map<String, Value> {
        value.as_object().cloned().unwrap()
    }

    #[test]
    fn parses_cache_ttls() {
        assert_eq!(parse_ttl(Some("5m")), Some(300_000.0));
        assert_eq!(parse_ttl(Some("1h")), Some(3_600_000.0));
        assert_eq!(parse_ttl(Some("30s")), Some(30_000.0));
        assert_eq!(parse_ttl(Some("250ms")), Some(250.0));
        assert_eq!(parse_ttl(Some("1d")), Some(86_400_000.0));
        assert_eq!(parse_ttl(Some("5 minutes")), None);
        assert_eq!(parse_ttl(None), None);
    }

    #[test]
    fn decides_like_upstream() {
        let on = Configured::Enabled(true);
        let settings = |interval: f64, ttl: f64| Settings { prompt: DEFAULT_PROMPT.into(), interval, duration: DEFAULT_DURATION_MS, ttl };
        assert_eq!(decide(None, None, "m"), Decision::Off);
        assert_eq!(decide(None, Some(&Configured::Enabled(false)), "m"), Decision::Off);
        assert_eq!(decide(None, Some(&on), "m"), Decision::Warm(settings(240_000.0, 300_000.0)));
        assert_eq!(decide(Some(&cache(json!({"ttl": "5m"}))), Some(&on), "m"), Decision::Warm(settings(270_000.0, 300_000.0)));
        assert_eq!(decide(Some(&cache(json!({"ttl": "1h"}))), Some(&on), "m"), Decision::CacheOutlivesWindow);
        // A model's `false` wins over the top level, its `true` turns warming on without one.
        assert_eq!(decide(Some(&cache(json!({"warming": false}))), Some(&on), "m"), Decision::Off);
        assert_eq!(decide(Some(&cache(json!({"warming": true}))), None, "m"), Decision::Warm(settings(240_000.0, 300_000.0)));
        // An invalid model setting falls back to the top level.
        assert_eq!(decide(Some(&cache(json!({"warming": {"interval": "soon"}}))), None, "m"), Decision::Off);
        assert_eq!(decide(Some(&cache(json!({"warming": {"interval": "soon"}}))), Some(&on), "m"), Decision::Warm(settings(240_000.0, 300_000.0)));
        // The model's timing overlays the top level's; the prompt comes from the top level.
        let global = Configured::Settings(WarmingInfo {
            prompt: Some("Reply pong".into()),
            interval: Some(WarmingInterval::Duration(ConfigDuration::minutes(2.0))),
            duration: None,
        });
        let decided = decide(Some(&cache(json!({"warming": {"duration": "1 hour"}}))), Some(&global), "m");
        assert_eq!(decided, Decision::Warm(Settings { prompt: "Reply pong".into(), interval: 120_000.0, duration: 3_600_000.0, ttl: 300_000.0 }));
        // An explicit interval longer than five minutes implies a cache lasting that long.
        let decided = decide(Some(&cache(json!({"warming": {"interval": "10 minutes"}}))), Some(&on), "m");
        assert_eq!(decided, Decision::Warm(settings(600_000.0, 600_000.0)));
        // An explicit interval as long as the window is rejected, even with "1h" caches.
        let decided = decide(Some(&cache(json!({"ttl": "1h", "warming": {"interval": "30 minutes"}}))), Some(&on), "m");
        assert_eq!(decided, Decision::InvalidInterval { interval: 1_800_000.0, duration: 1_800_000.0 });
        let decided = decide(Some(&cache(json!({"warming": {"interval": "0 millis"}}))), Some(&on), "m");
        assert_eq!(decided, Decision::InvalidInterval { interval: 0.0, duration: 1_800_000.0 });
        // An "auto" interval with an infinite window is invalid, not outlived.
        let decided = decide(Some(&cache(json!({"warming": {"duration": "Infinity"}}))), Some(&on), "m");
        assert!(matches!(decided, Decision::InvalidInterval { interval, duration } if interval == 240_000.0 && duration.is_infinite()), "{decided:?}");
    }

    #[test]
    fn reads_the_last_user_text() {
        use oc_ai::types::{ContentPart, Message, Role};
        assert_eq!(last_text(&[Message::user_text("a"), Message::user_text("b")]), Some("b"));
        assert_eq!(last_text(&[Message::new(Role::User, vec![ContentPart::text("a"), ContentPart::text("b")])]), None);
        assert_eq!(last_text(&[Message::assistant(vec![ContentPart::text("a")])]), None);
        assert_eq!(last_text(&[]), None);
    }
}
