use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use oc_core::coordinator::*;
use oc_core::inbox::Scope;
use parking_lot::Mutex;
use tokio_util::sync::CancellationToken;

#[derive(Default)]
struct D {
    drains: AtomicUsize,
    log: Mutex<Vec<String>>,
}

#[async_trait]
impl Driver for D {
    async fn started(&self, key: &str) {
        self.log.lock().push(format!("started {key}"));
    }
    async fn drain(&self, _key: &str, _force: bool, _scope: Scope, cancel: CancellationToken) -> Result<(), DrainError> {
        self.drains.fetch_add(1, Ordering::SeqCst);
        tokio::select! {
            _ = cancel.cancelled() => Err(DrainError::Interrupted),
            _ = tokio::time::sleep(Duration::from_millis(50)) => Ok(()),
        }
    }
    async fn settled(&self, key: &str, exit: &Exit, reason: Option<Reason>) {
        self.log.lock().push(format!("settled {key} {:?} {:?}", matches!(exit, Exit::Success), reason));
    }
}

#[tokio::test]
async fn wake_coalesces_into_second_drain() {
    let d = Arc::new(D::default());
    let c = Coordinator::new(d.clone());
    c.wake("s", Scope::Input);
    tokio::time::sleep(Duration::from_millis(10)).await;
    c.wake("s", Scope::Steer);
    c.wake("s", Scope::Steer);
    c.await_idle("s").await;
    assert_eq!(d.drains.load(Ordering::SeqCst), 2);
    assert_eq!(d.log.lock().clone(), vec!["started s", "settled s true None"]);
}

#[tokio::test]
async fn interrupt_settles_with_reason() {
    let d = Arc::new(D::default());
    let c = Coordinator::new(d.clone());
    c.wake("s", Scope::Input);
    tokio::time::sleep(Duration::from_millis(10)).await;
    assert!(c.interrupt("s", Some(Reason::User), true).await);
    assert!(!c.is_active("s"));
    assert_eq!(d.log.lock().last().unwrap(), "settled s false Some(User)");
    assert!(matches!(c.run("s").await, Exit::Success));
}
