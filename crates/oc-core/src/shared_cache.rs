//! `SharedCache` (`core/src/shared-cache.ts`): process-wide results of remote lists that several locations share,
//! such as GitHub Copilot's model list. A result stays fresh for an interval (10 minutes unless the caller passes
//! its own); concurrent callers join one load; transient failures retry; a failure keeps the last good value.

use std::any::Any;
use std::collections::HashMap;
use std::future::Future;
use std::sync::Arc;
use std::time::Duration;

use futures::future::{BoxFuture, FutureExt, Shared};
use once_cell::sync::Lazy;
use parking_lot::Mutex;
use tokio::time::Instant;

/// How long a result stays fresh, and how often callers refresh, unless a caller passes its own.
pub const INTERVAL: Duration = Duration::from_secs(10 * 60);
/// An interval that never expires (`Duration.infinity`): any stored result is returned, however old.
pub const FOREVER: Duration = Duration::MAX;
/// Entries nobody asked for this long (or for two of their intervals, if longer) are dropped.
const UNREAD: Duration = Duration::from_secs(24 * 60 * 60);
/// Transient failures retry this many times, after 1 s, 2 s and 4 s.
const RETRIES: u32 = 3;

/// The process-wide cache (upstream's global node).
pub static GLOBAL: Lazy<Arc<SharedCache>> = Lazy::new(|| Arc::new(SharedCache::default()));

/// A failed load. `transient` failures (network errors, timeouts and HTTP 408, 429, 500, 502, 503 or 504, as
/// Effect's `HttpClient.retryTransient` counts them) are retried.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LoadError {
    pub message: String,
    pub transient: bool,
}

impl LoadError {
    pub fn new(message: impl Into<String>, transient: bool) -> Self {
        LoadError { message: message.into(), transient }
    }

    /// A failed request: transport failures and timeouts are transient.
    pub fn request(error: &reqwest::Error) -> Self {
        LoadError::new(error.to_string(), true)
    }

    /// A non-2xx answer (`StatusCodeError`).
    pub fn status(status: u16) -> Self {
        LoadError::new(format!("StatusCode: non 2xx status code ({status})"), transient_status(status))
    }
}

impl std::fmt::Display for LoadError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

/// `SharedCache.transient` for an HTTP status.
pub fn transient_status(status: u16) -> bool {
    matches!(status, 408 | 429 | 500 | 502 | 503 | 504)
}

/// `SharedCache.fingerprint`: a short, one-way stand-in for a secret in a cache key.
pub fn fingerprint(secret: &str) -> String {
    use sha2::Digest;
    hex::encode(sha2::Sha256::digest(secret.as_bytes()))[..16].to_string()
}

type Stored = Arc<dyn Any + Send + Sync>;
/// `None`: the load was interrupted or panicked, which is never kept.
type Outcome = Option<Result<Stored, LoadError>>;

struct Entry {
    last: Option<(Result<Stored, LoadError>, Instant)>,
    loading: Option<Shared<BoxFuture<'static, Outcome>>>,
    read: Instant,
    unread: Duration,
}

#[derive(Default)]
pub struct SharedCache {
    entries: Mutex<HashMap<String, Entry>>,
}

impl SharedCache {
    /// Returns the last result for `key` while it is younger than `interval`, otherwise joins the running load or
    /// starts one, so loads never overlap per key. `force` skips the age check but still joins a running load.
    /// A load retries transient failures 3 times. A failure keeps the last good value, or without one is returned,
    /// until the next interval. Entries nobody asked for in 24 hours, or in two of their intervals if longer, are
    /// dropped. The key decides the value's type.
    pub async fn get<T, F, Fut>(self: &Arc<Self>, key: &str, interval: Duration, force: bool, load: F) -> Result<T, LoadError>
    where
        T: Clone + Send + Sync + 'static,
        F: Fn() -> Fut + Send + Sync + 'static,
        Fut: Future<Output = Result<T, LoadError>> + Send + 'static,
    {
        let shared = {
            let now = Instant::now();
            let mut entries = self.entries.lock();
            entries.retain(|_, item| item.loading.is_some() || now.saturating_duration_since(item.read) <= item.unread);
            let entry = entries.entry(key.to_string()).or_insert_with(|| Entry { last: None, loading: None, read: now, unread: UNREAD });
            entry.read = now;
            // A caller refreshing less often than daily must never find its entry dropped between two of its refreshes.
            if interval != FOREVER {
                entry.unread = entry.unread.max(interval.saturating_mul(2));
            }
            if !force
                && let Some((last, time)) = &entry.last
                && now.saturating_duration_since(*time) < interval
            {
                return downcast(last.clone());
            }
            match &entry.loading {
                Some(loading) => loading.clone(),
                None => {
                    let this = self.clone();
                    let owned = key.to_string();
                    let task = tokio::spawn(async move { this.start(&owned, load).await });
                    let shared = async move { task.await.ok().flatten() }.boxed().shared();
                    entry.loading = Some(shared.clone());
                    shared
                }
            }
        };
        match shared.await {
            Some(result) => downcast(result),
            None => Err(LoadError::new("The shared value's load was interrupted", false)),
        }
    }

    async fn start<T, F, Fut>(self: &Arc<Self>, key: &str, load: F) -> Outcome
    where
        T: Send + Sync + 'static,
        F: Fn() -> Fut + Send + Sync + 'static,
        Fut: Future<Output = Result<T, LoadError>> + Send + 'static,
    {
        let time = Instant::now();
        let mut attempt = 0;
        let result = loop {
            match load().await {
                Ok(value) => break Ok(Arc::new(value) as Stored),
                Err(error) if error.transient && attempt < RETRIES => {
                    tokio::time::sleep(Duration::from_secs(1 << attempt)).await;
                    attempt += 1;
                }
                Err(error) => break Err(error),
            }
        };
        if let Err(error) = &result {
            tracing::warn!(key, cause = %error, "failed to load shared value");
        }
        let mut entries = self.entries.lock();
        let entry = entries.get_mut(key)?;
        entry.read = Instant::now();
        entry.loading = None;
        let keep = match (&result, &entry.last) {
            (Err(_), Some((Ok(previous), _))) => Ok(previous.clone()),
            _ => result,
        };
        entry.last = Some((keep.clone(), time));
        Some(keep)
    }
}

fn downcast<T: Clone + 'static>(result: Result<Stored, LoadError>) -> Result<T, LoadError> {
    let value = result?;
    value.downcast_ref::<T>().cloned().ok_or_else(|| LoadError::new("The shared value has another type", false))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering};

    fn counter() -> Arc<AtomicU32> {
        Arc::new(AtomicU32::new(0))
    }

    // shared-cache.test.ts "joins a running load, refreshes after the caller's interval, and forces without
    // overlapping".
    #[tokio::test(start_paused = true)]
    async fn joins_refreshes_and_forces() {
        let cache = Arc::new(SharedCache::default());
        let (open_gate, gate) = tokio::sync::watch::channel(false);
        let get = |key: &'static str, interval: Duration, force: bool, loads: Arc<AtomicU32>| {
            let cache = cache.clone();
            let gate = gate.clone();
            async move {
                cache
                    .get(key, interval, force, move || {
                        let mut gate = gate.clone();
                        let loads = loads.clone();
                        async move {
                            let _ = gate.wait_for(|open| *open).await;
                            Ok::<_, LoadError>(format!("{key}{}", loads.fetch_add(1, Ordering::SeqCst) + 1))
                        }
                    })
                    .await
            }
        };
        let (a, b) = (counter(), counter());
        let joined = tokio::spawn(futures::future::join3(
            get("a", INTERVAL, false, a.clone()),
            get("a", INTERVAL, true, a.clone()),
            get("b", INTERVAL, false, b.clone()),
        ));
        tokio::task::yield_now().await;
        open_gate.send_replace(true);
        let (x, y, z) = joined.await.unwrap();
        assert_eq!((x.unwrap(), y.unwrap(), z.unwrap()), ("a1".to_string(), "a1".to_string(), "b1".to_string()));

        let open = |key: &'static str, interval: Duration, force: bool, loads: Arc<AtomicU32>| {
            let cache = cache.clone();
            async move {
                cache
                    .get(key, interval, force, move || {
                        let loads = loads.clone();
                        async move { Ok::<_, LoadError>(format!("{key}{}", loads.fetch_add(1, Ordering::SeqCst) + 1)) }
                    })
                    .await
                    .unwrap()
            }
        };
        tokio::time::advance(Duration::from_secs(5 * 60)).await;
        assert_eq!(open("a", INTERVAL, false, a.clone()).await, "a1");
        assert_eq!(open("a", Duration::from_secs(60), false, a.clone()).await, "a2");
        assert_eq!(open("a", INTERVAL, true, a.clone()).await, "a3");
        tokio::time::advance(Duration::from_secs(10 * 60)).await;
        assert_eq!(open("b", INTERVAL, false, b.clone()).await, "b2");
        assert_eq!((a.load(Ordering::SeqCst), b.load(Ordering::SeqCst)), (3, 2));
        // `Duration.infinity` returns what is stored, however old.
        tokio::time::advance(Duration::from_secs(60 * 60)).await;
        assert_eq!(open("b", FOREVER, false, b.clone()).await, "b2");
    }

    // shared-cache.test.ts "retries transient failures, then keeps the last list or the failure until the next
    // interval".
    #[tokio::test(start_paused = true)]
    async fn retries_and_keeps_the_last_value() {
        let cache = Arc::new(SharedCache::default());
        let results: Arc<Mutex<Vec<Result<String, LoadError>>>> = Arc::new(Mutex::new(vec![]));
        let loads = counter();
        let get = |force: bool| {
            let cache = cache.clone();
            let results = results.clone();
            let loads = loads.clone();
            async move {
                cache
                    .get("models", INTERVAL, force, move || {
                        let results = results.clone();
                        let loads = loads.clone();
                        async move {
                            loads.fetch_add(1, Ordering::SeqCst);
                            results.lock().remove(0)
                        }
                    })
                    .await
            }
        };
        let denied = LoadError::status(401);
        let timeout = LoadError::new("timeout", true);
        *results.lock() = vec![Err(denied.clone())];
        assert_eq!(get(false).await, Err(denied.clone()));
        tokio::time::advance(Duration::from_secs(9 * 60)).await;
        assert_eq!(get(false).await, Err(denied.clone()));
        *results.lock() = vec![Err(timeout.clone()), Err(timeout.clone()), Ok("list".into())];
        assert_eq!(get(true).await.unwrap(), "list");
        assert_eq!(loads.load(Ordering::SeqCst), 4);

        *results.lock() = vec![Err(denied.clone()), Err(timeout.clone()), Err(timeout.clone()), Err(timeout.clone()), Err(timeout.clone())];
        assert_eq!(get(true).await.unwrap(), "list");
        assert_eq!(get(true).await.unwrap(), "list");
        assert_eq!(get(false).await.unwrap(), "list");
        assert_eq!(loads.load(Ordering::SeqCst), 9);
    }

    // shared-cache.test.ts "drops entries unread for 24 hours, never one still in use or loading".
    #[tokio::test(start_paused = true)]
    async fn drops_unread_entries() {
        let cache = Arc::new(SharedCache::default());
        let once = |key: &'static str, loads: Arc<AtomicU32>| {
            move || {
                let loads = loads.clone();
                async move {
                    if loads.fetch_add(1, Ordering::SeqCst) == 0 { Ok(key.to_string()) } else { Err(LoadError::new("offline", false)) }
                }
            }
        };
        let (idle, used, daily) = (counter(), counter(), counter());
        assert_eq!(cache.get("idle", INTERVAL, false, once("idle", idle.clone())).await.unwrap(), "idle");
        let two_days = Duration::from_secs(48 * 60 * 60);
        assert_eq!(cache.get("daily", two_days, false, once("daily", daily.clone())).await.unwrap(), "daily");
        // A location refreshing every 10 minutes keeps its entry, so its list survives later failures.
        for _ in 0..=150 {
            assert_eq!(cache.get("used", INTERVAL, false, once("used", used.clone())).await.unwrap(), "used");
            tokio::time::advance(INTERVAL).await;
        }
        assert_eq!(cache.get("idle", INTERVAL, false, once("idle", idle.clone())).await, Err(LoadError::new("offline", false)));
        assert_eq!(idle.load(Ordering::SeqCst), 2);
        tokio::time::advance(Duration::from_secs(23 * 60 * 60)).await;
        assert_eq!(cache.get("daily", two_days, false, once("daily", daily.clone())).await.unwrap(), "daily");
    }

    #[test]
    fn transient_statuses_and_fingerprints() {
        for status in [408, 429, 500, 502, 503, 504] {
            assert!(LoadError::status(status).transient);
        }
        for status in [401, 404, 501] {
            assert!(!LoadError::status(status).transient);
        }
        assert_eq!(fingerprint("[]"), "4f53cda18c2baa0c");
    }
}
