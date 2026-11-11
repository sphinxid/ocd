//! Where a session's drain yields. Upstream forks background work (`Effect.forkIn`, a plugin's activation) behind
//! the running fiber: it runs once that fiber waits for something. A drain that a tool result resumes carries on
//! without waiting until its next step sends the provider request. Work that upstream runs at that point waits
//! here for the same moment ([`reached`], called by the step before the request), or for the execution's end.

use std::collections::HashMap;
use std::sync::LazyLock;
use std::time::Duration;

use parking_lot::Mutex;
use tokio::sync::{broadcast, oneshot};

use crate::bus::Event;

static WAITERS: LazyLock<Mutex<HashMap<String, Vec<oneshot::Sender<()>>>>> = LazyLock::new(Default::default);

/// How long work waits for the drain at most.
const CAP: Duration = Duration::from_secs(10);

/// The session's step assembled its provider request and now waits for the provider.
pub fn reached(session_id: &str) {
    let waiters = WAITERS.lock().remove(session_id);
    for waiter in waiters.into_iter().flatten() {
        let _ = waiter.send(());
    }
}

/// A wait for the session's next yield, registered now (before the drain can get there).
pub struct Yield {
    session: String,
    reached: oneshot::Receiver<()>,
    events: broadcast::Receiver<Event>,
}

impl Yield {
    pub fn register(bus: &crate::bus::Bus, session_id: &str) -> Yield {
        let (tx, reached) = oneshot::channel();
        let mut waiters = WAITERS.lock();
        // Waits that ended otherwise (the execution ended, the cap) leave closed senders behind.
        waiters.retain(|_, senders| {
            senders.retain(|s| !s.is_closed());
            !senders.is_empty()
        });
        waiters.entry(session_id.to_string()).or_default().push(tx);
        drop(waiters);
        Yield { session: session_id.to_string(), reached, events: bus.subscribe() }
    }

    /// Resolves at the session's next provider request, when its execution ends, or after a cap.
    pub async fn wait(self) {
        let Yield { session, reached, mut events } = self;
        let ended = async {
            loop {
                match events.recv().await {
                    Ok(event)
                        if event.kind.starts_with("session.execution.")
                            && event.kind != "session.execution.started"
                            && event.data["sessionID"] == session.as_str() =>
                    {
                        return;
                    }
                    Ok(_) | Err(broadcast::error::RecvError::Lagged(_)) => {}
                    Err(_) => return,
                }
            }
        };
        tokio::select! {
            _ = reached => {}
            _ = ended => {}
            _ = tokio::time::sleep(CAP) => {}
        }
    }
}
