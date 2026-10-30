//! OAuth refresh coordination (2.2 `packages/core/test/integration-refresh.test.ts` and
//! `credential-lock.test.ts`): concurrent resolves of one expiring credential share a single refresh, even
//! across locations and after the first caller goes away; failures are shared but a later resolve tries again;
//! a stuck refresh is bounded; the refresh runs under the cross-process file lock.

use async_trait::async_trait;
use oc_ai::credentials::form::FormAnswer;
use oc_ai::credentials::integration::{
    AuthorizeResult, FlowError, FlowResult, IntegrationError, Integrations, OAuthMethod, OAuthMethodInfo,
};
use oc_ai::credentials::store::{CreateCredential, CredentialStore, MemoryCredentialStore};
use oc_ai::credentials::{map_env, CredentialValue, OAuthValue};
use parking_lot::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Notify;
use tokio_util::sync::CancellationToken;

/// A refresh that waits for the test to release it.
struct Gated {
    calls: AtomicU64,
    entered: Notify,
    release: Notify,
    /// Fail the next refresh.
    fail: Mutex<bool>,
    /// Never finish (until the timeout).
    stuck: Mutex<bool>,
}

impl Gated {
    fn new() -> Arc<Self> {
        Arc::new(Self { calls: AtomicU64::new(0), entered: Notify::new(), release: Notify::new(), fail: Mutex::new(false), stuck: Mutex::new(false) })
    }
}

#[async_trait]
impl OAuthMethod for Gated {
    fn integration_id(&self) -> String {
        "openai".into()
    }
    fn info(&self) -> OAuthMethodInfo {
        OAuthMethodInfo::new("oauth", "OAuth")
    }
    async fn authorize(&self, _answer: FormAnswer, _scope: CancellationToken) -> FlowResult<AuthorizeResult> {
        Err(FlowError::new("unused sign-in"))
    }
    fn can_refresh(&self) -> bool {
        true
    }
    async fn refresh(&self, value: OAuthValue) -> FlowResult<OAuthValue> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        assert_eq!(value.refresh, "old-refresh");
        self.entered.notify_one();
        if *self.stuck.lock() {
            std::future::pending::<()>().await;
        }
        self.release.notified().await;
        if std::mem::take(&mut *self.fail.lock()) {
            return Err(FlowError::new("refresh rejected"));
        }
        Ok(OAuthValue { access: "new-access".into(), refresh: "new-refresh".into(), expires: u64::MAX / 2, ..value })
    }
}

/// Two location registries over one global credential store.
fn locations(method: &Arc<Gated>) -> (Arc<MemoryCredentialStore>, Arc<Integrations>, Arc<Integrations>) {
    let store = MemoryCredentialStore::shared();
    let clock: oc_ai::credentials::Clock = Arc::new(|| 1_000_000);
    let a = Integrations::with(store.clone(), map_env(std::iter::empty::<(String, String)>()), clock.clone());
    let b = Integrations::with(store.clone(), map_env(std::iter::empty::<(String, String)>()), clock);
    a.register_oauth(method.clone());
    b.register_oauth(method.clone());
    (store, a, b)
}

async fn expiring(store: &MemoryCredentialStore) -> String {
    let value = OAuthValue { method_id: "oauth".into(), refresh: "old-refresh".into(), access: "old-access".into(), expires: 1, metadata: None };
    store.create(CreateCredential::new("openai", value.into_value())).await.unwrap().id
}

async fn shares_refresh_across_locations(cancel: bool) {
    let method = Gated::new();
    let (store, a, b) = locations(&method);
    let id = expiring(&store).await;
    let connection = a.active("openai").await.unwrap().unwrap();
    let first = {
        let (a, connection) = (a.clone(), connection.clone());
        tokio::spawn(async move { a.resolve(&connection).await })
    };
    method.entered.notified().await;
    if cancel {
        first.abort();
    }
    let second = {
        let (b, connection) = (b.clone(), connection.clone());
        tokio::spawn(async move { b.resolve(&connection).await })
    };
    tokio::task::yield_now().await;
    assert_eq!(method.calls.load(Ordering::SeqCst), 1);
    method.release.notify_one();
    let value = second.await.unwrap().unwrap().unwrap();
    assert_eq!(value.as_oauth().unwrap().access, "new-access");
    if !cancel {
        assert_eq!(first.await.unwrap().unwrap(), Some(value.clone()));
    }
    assert_eq!(store.get(&id).await.unwrap().unwrap().value, value);
    assert_eq!(b.resolve(&connection).await.unwrap(), Some(value));
    assert_eq!(method.calls.load(Ordering::SeqCst), 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn shares_refresh_across_locations_test() {
    shares_refresh_across_locations(false).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn shares_refresh_across_locations_after_the_first_caller_is_interrupted() {
    shares_refresh_across_locations(true).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn shares_failures_but_allows_a_later_refresh() {
    let method = Gated::new();
    let (store, a, b) = locations(&method);
    expiring(&store).await;
    *method.fail.lock() = true;
    let connection = a.active("openai").await.unwrap().unwrap();
    let first = {
        let (a, connection) = (a.clone(), connection.clone());
        tokio::spawn(async move { a.resolve(&connection).await })
    };
    method.entered.notified().await;
    let second = {
        let (b, connection) = (b.clone(), connection.clone());
        tokio::spawn(async move { b.resolve(&connection).await })
    };
    tokio::task::yield_now().await;
    method.release.notify_one();
    let failure = IntegrationError::Authorization("refresh rejected".into());
    assert_eq!(first.await.unwrap().unwrap_err(), failure);
    assert_eq!(second.await.unwrap().unwrap_err(), failure);
    assert_eq!(method.calls.load(Ordering::SeqCst), 1);
    // A later resolve tries again.
    let third = {
        let (a, connection) = (a.clone(), connection.clone());
        tokio::spawn(async move { a.resolve(&connection).await })
    };
    method.entered.notified().await;
    method.release.notify_one();
    assert_eq!(third.await.unwrap().unwrap().unwrap().as_oauth().unwrap().access, "new-access");
    assert_eq!(method.calls.load(Ordering::SeqCst), 2);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn bounds_a_stuck_refresh_and_allows_a_later_attempt() {
    let method = Gated::new();
    let (store, a, _) = locations(&method);
    expiring(&store).await;
    *method.stuck.lock() = true;
    a.set_refresh_timeout(Duration::from_millis(100));
    let connection = a.active("openai").await.unwrap().unwrap();
    let error = a.resolve(&connection).await.unwrap_err();
    assert_eq!(error, IntegrationError::Authorization("Operation timed out after '100ms'".into()));
    *method.stuck.lock() = false;
    let later = {
        let (a, connection) = (a.clone(), connection.clone());
        tokio::spawn(async move { a.resolve(&connection).await })
    };
    method.entered.notified().await;
    method.release.notify_one();
    assert_eq!(later.await.unwrap().unwrap().unwrap().as_oauth().unwrap().access, "new-access");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn refreshes_under_the_cross_process_lock_and_rereads_the_stored_credential() {
    let method = Gated::new();
    let (store, a, _) = locations(&method);
    let id = expiring(&store).await;
    let root = tempfile::tempdir().unwrap();
    a.set_lock_root(Some(root.path().to_path_buf()));
    // Another process holds the lock and refreshes meanwhile.
    let other = oc_util::flock::acquire(root.path(), &format!("credential-refresh:{id}"), Default::default()).await.unwrap();
    let connection = a.active("openai").await.unwrap().unwrap();
    let waiting = {
        let (a, connection) = (a.clone(), connection.clone());
        tokio::spawn(async move { a.resolve(&connection).await })
    };
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(method.calls.load(Ordering::SeqCst), 0);
    let refreshed = OAuthValue { method_id: "oauth".into(), refresh: "other".into(), access: "other-access".into(), expires: u64::MAX / 2, metadata: None };
    store
        .update(&id, oc_ai::credentials::store::CredentialUpdate { label: None, value: Some(refreshed.clone().into_value()) })
        .await
        .unwrap();
    other.release().unwrap();
    let value: CredentialValue = waiting.await.unwrap().unwrap().unwrap();
    assert_eq!(value.as_oauth().unwrap().access, "other-access");
    assert_eq!(method.calls.load(Ordering::SeqCst), 0);
}
