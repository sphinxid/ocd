//! Panics go to the log file (upstream logs `uncaught exception` the same way), so a crash leaves a trace
//! even when stderr was a full-screen TUI or `/dev/null`.

use std::sync::OnceLock;
use std::thread::ThreadId;

/// Called before the message is printed: `(on the main thread) -> print it to stderr`.
type BeforePrint = fn(bool) -> bool;

static MAIN: OnceLock<ThreadId> = OnceLock::new();
static BEFORE_PRINT: OnceLock<BeforePrint> = OnceLock::new();

/// Install the hook; call from `main` (the thread recorded as the main thread).
pub fn install() {
    if MAIN.set(std::thread::current().id()).is_err() {
        return;
    }
    let print = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let thread = std::thread::current();
        let main = MAIN.get() == Some(&thread.id());
        let cause = match info.payload().downcast_ref::<&str>() {
            Some(s) => s.to_string(),
            None => info.payload().downcast_ref::<String>().cloned().unwrap_or_else(|| "Box<dyn Any>".into()),
        };
        let location = info.location().map(|l| format!("{}:{}:{}", l.file(), l.line(), l.column())).unwrap_or_default();
        tracing::error!(cause = %cause, location = %location, thread = thread.name().unwrap_or("<unnamed>"), main, "panicked");
        if BEFORE_PRINT.get().is_none_or(|f| f(main)) {
            print(info);
        }
    }));
}

/// Run `f` before a panic message is printed; it returns whether to print it. Set once.
pub fn before_print(f: BeforePrint) {
    let _ = BEFORE_PRINT.set(f);
}
