//! Stop signals for every command (`cli/src/index.ts`): `NodeRuntime.runMain` interrupts the main fiber on SIGINT
//! or SIGTERM and the process exits 130 (SIGINT) or 143 (SIGTERM) once the teardown is done; `ShutdownDeadline`
//! bounds that teardown (`shutdown.rs`) and turns SIGHUP into SIGTERM when the process has a terminal (closing the
//! terminal sends SIGHUP; without a terminal, as under nohup or for the background service, SIGHUP keeps its default
//! action).
//!
//! `install` starts listening; [`stopped`] resolves once a stop signal arrived. A command that cancels its own work
//! (`run`, `serve`) waits for it; for any other command `main` drops the command's future, which is what the fiber
//! interruption amounts to.

use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, Ordering};

use tokio::sync::watch;

static STOP: OnceLock<watch::Sender<Option<&'static str>>> = OnceLock::new();
/// Whether the command is interrupted at once (see `install`).
static INTERRUPT: AtomicBool = AtomicBool::new(false);

fn sender() -> &'static watch::Sender<Option<&'static str>> {
    STOP.get_or_init(|| watch::channel(None).0)
}

/// Whether SIGHUP is treated as SIGTERM (`process.stdin.isTTY || process.stdout.isTTY`).
pub fn hangup_stops() -> bool {
    crate::term::stdin_is_tty() || crate::term::stdout_is_tty()
}

/// Start listening for stop signals (inside the runtime). `interrupt`: the command is dropped at once on a stop
/// signal; should it be stuck in a blocking call that cannot be dropped, the process still ends after the time a
/// private server gets to stop (3 s) and a little more.
pub fn install(interrupt: bool) {
    let _ = sender();
    INTERRUPT.store(interrupt, Ordering::SeqCst);
    #[cfg(unix)]
    tokio::spawn(async {
        use tokio::signal::unix::{SignalKind, signal};
        let (Ok(mut int), Ok(mut term)) = (signal(SignalKind::interrupt()), signal(SignalKind::terminate())) else { return };
        let mut hup = if hangup_stops() { signal(SignalKind::hangup()).ok() } else { None };
        let name = tokio::select! {
            _ = int.recv() => "SIGINT",
            _ = term.recv() => "SIGTERM",
            _ = async { match hup.as_mut() { Some(s) => { s.recv().await; } None => std::future::pending().await } } => "SIGTERM",
        };
        stop(name);
    });
    #[cfg(not(unix))]
    tokio::spawn(async {
        if tokio::signal::ctrl_c().await.is_ok() {
            stop("SIGINT");
        }
    });
}

/// A stop signal arrived: start the bounded teardown and wake every waiter.
pub fn stop(name: &'static str) {
    if sender().borrow().is_some() {
        return;
    }
    crate::shutdown::begin(name);
    sender().send_replace(Some(name));
    if INTERRUPT.load(Ordering::SeqCst) {
        std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_millis(3500));
            use std::io::Write;
            let _ = std::io::stdout().flush();
            let _ = std::io::stderr().flush();
            std::process::exit(exit_code(name));
        });
    }
}

/// The stop signal received so far, if any.
pub fn received() -> Option<&'static str> {
    *sender().borrow()
}

/// Resolves with the signal's name once a stop signal arrived.
pub async fn stopped() -> &'static str {
    let mut rx = sender().subscribe();
    loop {
        if let Some(name) = *rx.borrow_and_update() {
            return name;
        }
        if rx.changed().await.is_err() {
            return std::future::pending().await;
        }
    }
}

static DEFERRED: parking_lot::Mutex<Vec<tokio::task::JoinHandle<()>>> = parking_lot::Mutex::new(Vec::new());

/// A finalizer of an interrupted command (started from a `Drop`), which `main` waits for before exiting.
pub fn defer(task: impl std::future::Future<Output = ()> + Send + 'static) {
    if let Ok(handle) = tokio::runtime::Handle::try_current() {
        DEFERRED.lock().push(handle.spawn(task));
    }
}

/// Wait for the finalizers [`defer`] started.
pub async fn finish_deferred() {
    let tasks: Vec<_> = std::mem::take(&mut *DEFERRED.lock());
    for task in tasks {
        let _ = task.await;
    }
}

/// `defaultTeardown`'s code for an interrupted process: 130, or 143 after a SIGTERM.
pub fn exit_code(name: &str) -> i32 {
    if name == "SIGTERM" { 143 } else { 130 }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn codes() {
        assert_eq!(exit_code("SIGINT"), 130);
        assert_eq!(exit_code("SIGTERM"), 143);
    }
}
