//! `ShutdownDeadline` (`cli/src/shutdown-deadline.ts`): once a stop signal starts the teardown, it gets 10 s;
//! after that the process reports what it is stuck on and exits 1. A second signal during teardown reports
//! and exits at once. The report lists the pending work `track` recorded (`@opencode/util/teardown`): ocd
//! records the child processes it waits for (the private standalone server); upstream's `LayerNode` layer
//! releases have no counterpart.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};

use parking_lot::Mutex;

pub const DEADLINE: Duration = Duration::from_secs(10);
/// The file logger batches writes for up to a second.
const LOG_FLUSH: Duration = Duration::from_millis(1500);

static STARTED: AtomicBool = AtomicBool::new(false);
static ENTRIES: Mutex<Vec<(u64, String, Instant)>> = Mutex::new(Vec::new());
static NEXT: AtomicU64 = AtomicU64::new(0);

/// Pending work recorded by [`track`]; dropping it settles the entry.
pub struct Tracked(u64);

impl Drop for Tracked {
    fn drop(&mut self) {
        ENTRIES.lock().retain(|(id, _, _)| *id != self.0);
    }
}

/// `Teardown.track(label)`: record pending work until the returned guard is dropped.
pub fn track(label: impl Into<String>) -> Tracked {
    let id = NEXT.fetch_add(1, Ordering::SeqCst);
    ENTRIES.lock().push((id, label.into(), Instant::now()));
    Tracked(id)
}

/// `Teardown.pending()`: oldest first, with how long each has been pending.
fn pending() -> Vec<(String, Duration)> {
    ENTRIES.lock().iter().map(|(_, label, since)| (label.clone(), since.elapsed())).collect()
}

/// The stderr report: the reason, then the pending teardowns, then a blank line.
pub fn report(reason: &str) -> String {
    report_with(reason, &pending())
}

fn report_with(reason: &str, pending: &[(String, Duration)]) -> String {
    let mut lines = vec![format!("{reason}; exiting. Still pending:")];
    if pending.is_empty() {
        lines.push("  nothing tracked; the stuck finalizer is outside any LayerNode layer".into());
    }
    for (label, since) in pending {
        lines.push(format!("  {:.1}s  {label}", since.as_millis() as f64 / 1000.0));
    }
    lines.push(String::new());
    lines.join("\n")
}

/// `Duration.format`: whole seconds as `10s`.
fn format(d: Duration) -> String {
    format!("{}s", d.as_secs())
}

/// A stop signal started the teardown: arm the deadline and exit on a second signal.
pub fn begin(signal: &str) {
    begin_with(signal, DEADLINE);
}

pub fn begin_with(signal: &str, deadline: Duration) {
    if STARTED.swap(true, Ordering::SeqCst) {
        return;
    }
    tracing::info!(signal, "shutdown started");
    std::thread::spawn(move || {
        std::thread::sleep(deadline);
        let pending = pending();
        crate::term::write_stderr(&report_with(&format!("shutdown did not finish within {}", format(deadline)), &pending));
        let listed: Vec<String> = pending.iter().map(|(label, since)| format!("{label} for {}ms", since.as_millis())).collect();
        tracing::error!(deadline = %format(deadline), pending = %listed.join(","), "shutdown deadline exceeded");
        std::thread::sleep(LOG_FLUSH);
        std::process::exit(1);
    });
    #[cfg(unix)]
    if let Ok(handle) = tokio::runtime::Handle::try_current() {
        handle.spawn(async {
            use tokio::signal::unix::{SignalKind, signal};
            let (Ok(mut int), Ok(mut term)) = (signal(SignalKind::interrupt()), signal(SignalKind::terminate())) else { return };
            // With a terminal, SIGHUP is sent on as SIGTERM (`onHangup`).
            let mut hup = if crate::signals::hangup_stops() { signal(SignalKind::hangup()).ok() } else { None };
            let name = tokio::select! {
                _ = int.recv() => "SIGINT",
                _ = term.recv() => "SIGTERM",
                _ = async { match hup.as_mut() { Some(s) => { s.recv().await; } None => std::future::pending().await } } => "SIGTERM",
            };
            crate::term::write_stderr(&report(&format!("received {name} again during shutdown")));
            std::process::exit(1);
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reports_the_nothing_tracked_form() {
        assert_eq!(
            report_with("shutdown did not finish within 10s", &[]),
            "shutdown did not finish within 10s; exiting. Still pending:\n  nothing tracked; the stuck finalizer is outside any LayerNode layer\n"
        );
        assert_eq!(format(DEADLINE), "10s");
    }

    #[test]
    fn reports_tracked_work() {
        assert_eq!(
            report_with("received SIGINT again during shutdown", &[("child 7 running: /bin/ocd serve --stdio --port 0".into(), Duration::from_millis(3849))]),
            "received SIGINT again during shutdown; exiting. Still pending:\n  3.8s  child 7 running: /bin/ocd serve --stdio --port 0\n"
        );
        let a = track("a");
        let b = track("b");
        drop(a);
        assert_eq!(pending().iter().map(|(l, _)| l.as_str()).collect::<Vec<_>>(), ["b"]);
        drop(b);
    }
}
