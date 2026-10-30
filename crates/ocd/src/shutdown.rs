//! `ShutdownDeadline` (`cli/src/shutdown-deadline.ts`): once a stop signal starts the teardown, it gets 10 s;
//! after that the process reports what it is stuck on and exits 1. A second signal during teardown reports
//! and exits at once. ocd tracks no teardown labels (upstream's `LayerNode` layers), so the report always
//! takes the "nothing tracked" form.

use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

pub const DEADLINE: Duration = Duration::from_secs(10);
/// The file logger batches writes for up to a second.
const LOG_FLUSH: Duration = Duration::from_millis(1500);

static STARTED: AtomicBool = AtomicBool::new(false);

/// The stderr report: the reason, then the pending teardowns (none tracked), then a blank line.
pub fn report(reason: &str) -> String {
    [&format!("{reason}; exiting. Still pending:"), "  nothing tracked; the stuck finalizer is outside any LayerNode layer", ""].join("\n")
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
        crate::term::write_stderr(&report(&format!("shutdown did not finish within {}", format(deadline))));
        tracing::error!(deadline = %format(deadline), pending = "", "shutdown deadline exceeded");
        std::thread::sleep(LOG_FLUSH);
        std::process::exit(1);
    });
    #[cfg(unix)]
    if let Ok(handle) = tokio::runtime::Handle::try_current() {
        handle.spawn(async {
            use tokio::signal::unix::{SignalKind, signal};
            let (Ok(mut int), Ok(mut term)) = (signal(SignalKind::interrupt()), signal(SignalKind::terminate())) else { return };
            let name = tokio::select! {
                _ = int.recv() => "SIGINT",
                _ = term.recv() => "SIGTERM",
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
            report("shutdown did not finish within 10s"),
            "shutdown did not finish within 10s; exiting. Still pending:\n  nothing tracked; the stuck finalizer is outside any LayerNode layer\n"
        );
        assert_eq!(format(DEADLINE), "10s");
    }
}
