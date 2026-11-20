//! `WarmingPlugin` (`core/test/plugin/warming.test.ts`, 2.3.1) on paused time: the schedule (`"auto"` from the
//! model's `cache.ttl`, explicit intervals), cache misses, timing around running requests and compactions, and the
//! loop's lifecycle. Each ping records the minute it was sent and reports the next cache usage in `usage`.

use std::sync::Arc;
use std::time::Duration;

use futures::future::BoxFuture;
use oc_config::decode::Decode;
use oc_config::schema::Warming as Configured;
use oc_core::warming::{CacheUsage, Generator, Hook, Warming};
use parking_lot::Mutex;
use serde_json::{Value, json};
use tokio::time::Instant;

const SESSION: &str = "ses_warming_test";
const KEY: &str = "/project";

const HIT: CacheUsage = CacheUsage { read: 1000, write: 10 };
const MISS: CacheUsage = CacheUsage { read: 0, write: 1000 };
const UNREPORTED: CacheUsage = CacheUsage { read: 0, write: 0 };

struct Pings {
    start: Instant,
    warms: Mutex<Vec<f64>>,
    usage: Vec<CacheUsage>,
    ping: Duration,
}

struct Mock(Arc<Pings>);

impl Generator for Mock {
    fn generate(&self, _: String, _: String) -> BoxFuture<'static, anyhow::Result<CacheUsage>> {
        let pings = self.0.clone();
        Box::pin(async move {
            let cache = {
                let mut warms = pings.warms.lock();
                let cache = pings.usage.get(warms.len()).copied().unwrap_or(UNREPORTED);
                warms.push(pings.start.elapsed().as_secs_f64() / 60.0);
                cache
            };
            tokio::time::sleep(pings.ping).await;
            Ok(cache)
        })
    }
}

struct Run {
    warming: Arc<Warming>,
    pings: Arc<Pings>,
    global: Option<Configured>,
    /// The test model's `settings.cache` (changeable between requests).
    cache: Arc<Mutex<Option<Value>>>,
}

/// Runs the warming plugin with the given top-level `warming` and the test model's `cache`.
fn run(warming: Value, cache: Option<Value>, usage: &[CacheUsage], ping: Duration) -> Run {
    let pings = Arc::new(Pings { start: Instant::now(), warms: Mutex::new(vec![]), usage: usage.to_vec(), ping });
    let plugin = Warming::new();
    plugin.set_generator(Arc::new(Mock(pings.clone())));
    Run { warming: plugin, pings, global: Configured::decode(&warming), cache: Arc::new(Mutex::new(cache)) }
}

impl Run {
    fn hook(&self, tracked: bool) {
        self.warming.hook(Hook {
            key: KEY.into(),
            session_id: SESSION.into(),
            tracked,
            child: false,
            cache: self.cache.lock().as_ref().and_then(Value::as_object).cloned(),
            global: self.global.clone(),
            model: "test/test-model".into(),
            text: None,
        });
    }

    /// A request being sent: the step's context hook.
    fn send(&self) {
        self.hook(true);
    }

    /// A compaction request being sent.
    fn compact(&self) {
        self.hook(true);
    }

    /// The turn ending.
    async fn end(&self) {
        self.emit("session.execution.succeeded").await;
    }

    /// The compaction ending without a summary.
    async fn compaction_failed(&self) {
        self.emit("session.compaction.failed").await;
    }

    async fn remove(&self) {
        self.emit("session.deleted").await;
    }

    async fn emit(&self, kind: &str) {
        self.warming.event(kind, SESSION);
        settle().await;
    }

    /// One whole turn whose response ends right away.
    async fn turn(&self) {
        self.send();
        self.end().await;
    }

    /// Moves the clock to `minutes` after the start, waking every timer due on the way at its own time.
    async fn at(&self, minutes: f64) {
        tokio::time::sleep_until(self.pings.start + Duration::from_secs_f64(minutes * 60.0)).await;
        settle().await;
    }

    fn warms(&self) -> Vec<f64> {
        self.pings.warms.lock().clone()
    }
}

/// Lets the tasks woken at the current instant run.
async fn settle() {
    for _ in 0..50 {
        tokio::task::yield_now().await;
    }
}

const NONE: Duration = Duration::ZERO;

// ---------------------------------------------------------------------------------------------- interval

#[tokio::test(start_paused = true)]
async fn warms_every_4_minutes_when_the_model_has_no_cache_ttl() {
    let w = run(json!(true), None, &[], NONE);
    w.turn().await;
    for m in [4.0, 8.0] {
        w.at(m).await;
    }
    assert_eq!(w.warms(), [4.0, 8.0]);
}

#[tokio::test(start_paused = true)]
async fn warms_at_90_percent_of_the_models_cache_ttl_by_default() {
    let w = run(json!(true), Some(json!({"ttl": "5m"})), &[], NONE);
    w.turn().await;
    for m in [4.0, 4.5, 9.0] {
        w.at(m).await;
    }
    assert_eq!(w.warms(), [4.5, 9.0]);
}

#[tokio::test(start_paused = true)]
async fn doesnt_warm_a_cache_that_outlives_the_warming_window() {
    let w = run(json!(true), Some(json!({"ttl": "1h"})), &[], NONE);
    w.turn().await;
    for m in [4.0, 27.0, 54.0] {
        w.at(m).await;
    }
    assert!(w.warms().is_empty());
}

#[tokio::test(start_paused = true)]
async fn keeps_an_explicit_interval() {
    let w = run(json!({"interval": "2 minutes"}), Some(json!({"ttl": "5m"})), &[], NONE);
    w.turn().await;
    for m in [2.0, 4.0] {
        w.at(m).await;
    }
    assert_eq!(w.warms(), [2.0, 4.0]);
}

// ---------------------------------------------------------------------------------------------- cache miss

#[tokio::test(start_paused = true)]
async fn keeps_warming_after_a_ping_hits_the_cache() {
    let w = run(json!(true), None, &[HIT, HIT, HIT], NONE);
    w.turn().await;
    for m in [4.0, 8.0, 12.0] {
        w.at(m).await;
    }
    assert_eq!(w.warms(), [4.0, 8.0, 12.0]);
}

#[tokio::test(start_paused = true)]
async fn stops_warming_after_a_ping_misses_the_cache() {
    let w = run(json!(true), None, &[HIT, MISS, HIT], NONE);
    w.turn().await;
    for m in [4.0, 8.0, 12.0, 16.0] {
        w.at(m).await;
    }
    assert_eq!(w.warms(), [4.0, 8.0]);
}

#[tokio::test(start_paused = true)]
async fn resumes_warming_on_the_next_real_request_after_a_miss() {
    let w = run(json!(true), None, &[MISS, HIT, HIT], NONE);
    w.turn().await;
    for m in [4.0, 8.0] {
        w.at(m).await;
    }
    w.turn().await;
    for m in [12.0, 16.0] {
        w.at(m).await;
    }
    assert_eq!(w.warms(), [4.0, 12.0, 16.0]);
}

#[tokio::test(start_paused = true)]
async fn doesnt_count_a_ping_without_cache_usage_as_a_miss() {
    let w = run(json!(true), None, &[UNREPORTED, UNREPORTED], NONE);
    w.turn().await;
    for m in [4.0, 8.0] {
        w.at(m).await;
    }
    assert_eq!(w.warms(), [4.0, 8.0]);
}

#[tokio::test(start_paused = true)]
async fn keeps_warming_when_a_request_is_sent_while_a_missed_ping_responds() {
    let w = run(json!(true), Some(json!({"ttl": "5m"})), &[MISS, HIT], Duration::from_secs(30));
    w.turn().await;
    w.at(4.5).await;
    w.at(4.75).await;
    w.turn().await;
    for m in [5.0, 9.25] {
        w.at(m).await;
    }
    assert_eq!(w.warms(), [4.5, 9.25]);
}

// ---------------------------------------------------------------------------------------------- timing

#[tokio::test(start_paused = true)]
async fn counts_the_interval_from_when_the_request_was_sent_not_when_its_response_ended() {
    let w = run(json!(true), Some(json!({"ttl": "5m"})), &[], NONE);
    w.send();
    w.at(1.0).await;
    w.end().await;
    for m in [4.5, 5.5] {
        w.at(m).await;
    }
    assert_eq!(w.warms(), [4.5]);
}

#[tokio::test(start_paused = true)]
async fn counts_the_next_interval_from_when_a_ping_was_sent() {
    let w = run(json!(true), Some(json!({"ttl": "5m"})), &[], Duration::from_secs(30));
    w.turn().await;
    for m in [4.5, 9.0, 9.5] {
        w.at(m).await;
    }
    assert_eq!(w.warms(), [4.5, 9.0]);
}

#[tokio::test(start_paused = true)]
async fn waits_for_a_request_running_at_the_due_time_then_pings_while_the_cache_lasts() {
    let w = run(json!(true), Some(json!({"ttl": "5m"})), &[], NONE);
    w.send();
    w.at(4.5).await;
    assert!(w.warms().is_empty());
    w.at(4.75).await;
    w.end().await;
    for m in [9.0, 9.25] {
        w.at(m).await;
    }
    assert_eq!(w.warms(), [4.75, 9.25]);
}

#[tokio::test(start_paused = true)]
async fn stops_warming_when_a_request_outlasts_the_cache_until_the_next_request() {
    let w = run(json!(true), Some(json!({"ttl": "5m"})), &[], NONE);
    w.send();
    w.at(6.0).await;
    w.end().await;
    w.at(15.0).await;
    assert!(w.warms().is_empty());
    w.turn().await;
    w.at(19.5).await;
    assert_eq!(w.warms(), [19.5]);
}

// ---------------------------------------------------------------------------------------------- lifecycle

#[tokio::test(start_paused = true)]
async fn keeps_one_loop_when_warming_is_turned_off_and_on_again_before_the_next_ping() {
    let w = run(json!(false), Some(json!({"warming": true})), &[], Duration::from_secs(30));
    w.turn().await;
    w.at(1.0).await;
    *w.cache.lock() = Some(json!({"warming": false}));
    w.turn().await;
    w.at(2.0).await;
    *w.cache.lock() = Some(json!({"warming": true}));
    w.turn().await;
    for m in 3..=40 {
        w.at(m as f64).await;
    }
    assert_eq!(w.warms(), [6.0, 10.0, 14.0, 18.0, 22.0, 26.0, 30.0]);
}

#[tokio::test(start_paused = true)]
async fn doesnt_ping_during_a_running_compaction() {
    let w = run(json!(true), Some(json!({"ttl": "5m"})), &[], NONE);
    w.compact();
    w.at(4.5).await;
    assert!(w.warms().is_empty());
    w.at(4.75).await;
    w.compaction_failed().await;
    for m in [9.0, 9.25] {
        w.at(m).await;
    }
    assert_eq!(w.warms(), [4.75, 9.25]);
}

#[tokio::test(start_paused = true)]
async fn stops_warming_a_deleted_session() {
    let w = run(json!(true), None, &[], NONE);
    w.turn().await;
    w.at(4.0).await;
    w.remove().await;
    for m in [8.0, 12.0] {
        w.at(m).await;
    }
    assert_eq!(w.warms(), [4.0]);
    assert!(!w.warming.warming(SESSION));
}

#[tokio::test(start_paused = true)]
async fn stops_every_loop_when_the_plugin_closes() {
    let w = run(json!(true), None, &[], NONE);
    w.turn().await;
    w.warming.close(KEY);
    for m in [4.0, 8.0] {
        w.at(m).await;
    }
    assert!(w.warms().is_empty());
}

// ---------------------------------------------------------------------------------------------- ocd additions

/// The window counts from the latest real request: the default 30 minutes allow seven pings without a `cache.ttl`
/// (warming.mdx "Costs"), and the loop ends by itself after the window.
#[tokio::test(start_paused = true)]
async fn stops_when_the_window_closes() {
    let w = run(json!(true), None, &[], NONE);
    w.turn().await;
    for m in (4..=40).step_by(4) {
        w.at(m as f64).await;
    }
    assert_eq!(w.warms(), [4.0, 8.0, 12.0, 16.0, 20.0, 24.0, 28.0]);
    assert!(!w.warming.warming(SESSION));
}

/// A subagent's session is never warmed, and our own ping (its prompt as the last message) doesn't restart the loop.
#[tokio::test(start_paused = true)]
async fn skips_subagents_and_its_own_pings() {
    let w = run(json!(true), None, &[], NONE);
    let hook = |child: bool, text: Option<&str>| Hook {
        key: KEY.into(),
        session_id: SESSION.into(),
        tracked: false,
        child,
        cache: None,
        global: w.global.clone(),
        model: "test/test-model".into(),
        text: text.map(String::from),
    };
    w.warming.hook(hook(true, None));
    assert!(!w.warming.warming(SESSION));
    w.turn().await;
    w.at(3.0).await;
    // A ping-shaped generate request at 3 minutes leaves the loop as it was (next ping at 4).
    w.warming.hook(hook(false, Some(oc_core::warming::DEFAULT_PROMPT)));
    w.at(4.0).await;
    // Any other generate request restarts it (next ping 4 minutes after it).
    w.warming.hook(hook(false, Some("Judge the goal")));
    for m in [8.0, 9.0] {
        w.at(m).await;
    }
    assert_eq!(w.warms(), [4.0, 8.0]);
}

/// A failed ping is logged and the loop tries again an interval after it was sent.
#[tokio::test(start_paused = true)]
async fn retries_after_a_failed_ping() {
    struct Failing(Arc<Pings>);
    impl Generator for Failing {
        fn generate(&self, _: String, _: String) -> BoxFuture<'static, anyhow::Result<CacheUsage>> {
            let pings = self.0.clone();
            Box::pin(async move {
                pings.warms.lock().push(pings.start.elapsed().as_secs_f64() / 60.0);
                anyhow::bail!("provider unavailable")
            })
        }
    }
    let w = run(json!(true), None, &[], NONE);
    w.warming.set_generator(Arc::new(Failing(w.pings.clone())));
    w.turn().await;
    for m in [4.0, 8.0] {
        w.at(m).await;
    }
    assert_eq!(w.warms(), [4.0, 8.0]);
}
