//! Per-session busy-period coordinator (`core/src/session/run-coordinator.ts`).
//! Effect fiber interruption is modelled with a `CancellationToken` passed to `drain`.

use std::collections::HashMap;
use std::sync::Arc;

use async_trait::async_trait;
use parking_lot::Mutex;
use tokio::sync::watch;
use tokio_util::sync::CancellationToken;

use crate::inbox::Scope;

#[derive(Debug, Clone)]
pub enum Exit {
    Success,
    Failure(Arc<anyhow::Error>),
    Interrupted,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Reason {
    User,
    Shutdown,
    Inactivity,
}

#[derive(Debug)]
pub enum DrainError {
    Interrupted,
    Failed(anyhow::Error),
}

impl<E: Into<anyhow::Error>> From<E> for DrainError {
    fn from(e: E) -> Self {
        DrainError::Failed(e.into())
    }
}

#[async_trait]
pub trait Driver: Send + Sync + 'static {
    async fn started(&self, key: &str);
    async fn drain(&self, key: &str, force: bool, scope: Scope, cancel: CancellationToken) -> Result<(), DrainError>;
    async fn settled(&self, key: &str, exit: &Exit, reason: Option<Reason>);
}

struct Execution {
    done: watch::Sender<Option<Exit>>,
    cancel: CancellationToken,
    scope: Scope,
    pending_wake: Option<Scope>,
    stopping: bool,
    owner_alive: bool,
    reason: Option<Reason>,
}

type Exec = Arc<Mutex<Execution>>;

#[derive(Clone)]
pub struct Coordinator {
    inner: Arc<Inner>,
}

struct Inner {
    driver: Arc<dyn Driver>,
    executions: Mutex<HashMap<String, Exec>>,
}

impl Coordinator {
    pub fn new(driver: Arc<dyn Driver>) -> Self {
        Coordinator { inner: Arc::new(Inner { driver, executions: Mutex::new(HashMap::new()) }) }
    }

    pub fn active(&self) -> Vec<String> {
        self.inner.executions.lock().keys().cloned().collect()
    }

    pub fn is_active(&self, key: &str) -> bool {
        self.inner.executions.lock().contains_key(key)
    }

    fn start(&self, key: &str, force: bool, scope: Scope) -> Exec {
        let (tx, _) = watch::channel(None);
        let exec = Arc::new(Mutex::new(Execution {
            done: tx,
            cancel: CancellationToken::new(),
            scope,
            pending_wake: None,
            stopping: false,
            owner_alive: true,
            reason: None,
        }));
        self.inner.executions.lock().insert(key.to_string(), exec.clone());
        let this = self.clone();
        let key = key.to_string();
        let e = exec.clone();
        tokio::spawn(async move {
            tokio::task::yield_now().await;
            let driver = this.inner.driver.clone();
            driver.started(&key).await;
            let cancel = e.lock().cancel.clone();
            let mut force = force;
            let exit = loop {
                let scope = e.lock().scope;
                let result = if cancel.is_cancelled() {
                    Err(DrainError::Interrupted)
                } else {
                    driver.drain(&key, force, scope, cancel.clone()).await
                };
                match result {
                    Ok(()) => {
                        let mut g = e.lock();
                        if g.stopping {
                            break Exit::Interrupted;
                        }
                        match g.pending_wake.take() {
                            Some(next) => {
                                g.scope = next;
                                force = false;
                            }
                            None => break Exit::Success,
                        }
                    }
                    Err(DrainError::Interrupted) => break Exit::Interrupted,
                    Err(DrainError::Failed(err)) => {
                        break if cancel.is_cancelled() { Exit::Interrupted } else { Exit::Failure(Arc::new(err)) };
                    }
                }
                tokio::task::yield_now().await;
            };
            let reason = {
                let mut g = e.lock();
                g.owner_alive = false;
                g.reason
            };
            driver.settled(&key, &exit, reason).await;
            this.settle(&key, &e, exit);
        });
        exec
    }

    fn settle(&self, key: &str, exec: &Exec, exit: Exit) {
        let pending = exec.lock().pending_wake;
        {
            let mut map = self.inner.executions.lock();
            if map.get(key).is_some_and(|x| Arc::ptr_eq(x, exec)) {
                map.remove(key);
            }
        }
        if let Some(scope) = pending {
            self.start(key, false, scope);
        }
        let _ = exec.lock().done.send_replace(Some(exit));
    }

    async fn wait(exec: &Exec) -> Exit {
        let mut rx = exec.lock().done.subscribe();
        loop {
            if let Some(exit) = rx.borrow().clone() {
                return exit;
            }
            if rx.changed().await.is_err() {
                return Exit::Interrupted;
            }
        }
    }

    /// `run`: start while idle, or join the active execution.
    pub async fn run(&self, key: &str) -> Exit {
        loop {
            let existing = self.inner.executions.lock().get(key).cloned();
            match existing {
                Some(exec) => {
                    let stopping = exec.lock().stopping;
                    let exit = Self::wait(&exec).await;
                    if !stopping {
                        return exit;
                    }
                }
                None => {
                    let exec = self.start(key, true, Scope::Input);
                    return Self::wait(&exec).await;
                }
            }
        }
    }

    /// Ring the doorbell (widest scope wins).
    pub fn wake(&self, key: &str, scope: Scope) {
        let existing = self.inner.executions.lock().get(key).cloned();
        match existing {
            Some(exec) => {
                let mut g = exec.lock();
                g.pending_wake = Some(if g.pending_wake == Some(Scope::Input) { Scope::Input } else { scope });
            }
            None => {
                self.start(key, false, scope);
            }
        }
    }

    pub async fn interrupt(&self, key: &str, reason: Option<Reason>, await_settlement: bool) -> bool {
        let Some(exec) = self.inner.executions.lock().get(key).cloned() else { return false };
        let interrupted = {
            let mut g = exec.lock();
            if g.stopping {
                false
            } else if !g.owner_alive {
                g.pending_wake = None;
                false
            } else {
                g.stopping = true;
                g.pending_wake = None;
                g.reason = reason;
                g.cancel.cancel();
                true
            }
        };
        if await_settlement {
            Self::wait(&exec).await;
        }
        interrupted
    }

    pub async fn await_idle(&self, key: &str) {
        loop {
            let Some(exec) = self.inner.executions.lock().get(key).cloned() else { return };
            Self::wait(&exec).await;
        }
    }
}
