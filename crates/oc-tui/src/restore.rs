//! Put the terminal back when the TUI ends without reaching its own cleanup. OpenTUI destroys the renderer on
//! its `exitSignals` (SIGINT, SIGTERM, SIGQUIT, SIGABRT, SIGHUP, SIGPIPE, SIGBUS) and on an uncaught exception;
//! without that a killed or crashed TUI leaves raw mode and mouse reporting on, and every mouse move types
//! escape codes into the shell.
//!
//! The signal path only uses `write` and `tcsetattr`, which are async-signal-safe.

use std::sync::OnceLock;
use std::sync::atomic::{AtomicI32, AtomicU8, Ordering};

pub const ALT_SCREEN: u8 = 1;
pub const MOUSE: u8 = 2;
pub const FOCUS: u8 = 4;
pub const PASTE: u8 = 8;
pub const KITTY: u8 = 16;

/// The modes the TUI turned on; 0 while it is not holding the terminal.
static MODES: AtomicU8 = AtomicU8::new(0);
static TTY: AtomicI32 = AtomicI32::new(-1);
#[cfg(unix)]
static SAVED: OnceLock<libc::termios> = OnceLock::new();
static INSTALLED: OnceLock<()> = OnceLock::new();

/// Call before turning the modes on (and before raw mode).
pub fn arm(modes: u8) {
    #[cfg(unix)]
    save_termios();
    INSTALLED.get_or_init(install);
    MODES.store(modes, Ordering::SeqCst);
}

/// The TUI restored the terminal itself.
pub fn disarm() {
    MODES.store(0, Ordering::SeqCst);
}

pub fn armed() -> bool {
    MODES.load(Ordering::SeqCst) != 0
}

/// Turn the modes off and restore the terminal settings saved by the first [`arm`].
pub fn restore_now() {
    let modes = MODES.swap(0, Ordering::SeqCst);
    if modes == 0 {
        return;
    }
    if modes & KITTY != 0 {
        out(b"\x1b[<1u");
    }
    if modes & MOUSE != 0 {
        out(b"\x1b[?1006l\x1b[?1015l\x1b[?1003l\x1b[?1002l\x1b[?1000l");
    }
    if modes & FOCUS != 0 {
        out(b"\x1b[?1004l");
    }
    if modes & PASTE != 0 {
        out(b"\x1b[?2004l");
    }
    out(b"\x1b[?2026l\x1b[0m\x1b[?25h");
    if modes & ALT_SCREEN != 0 {
        out(b"\x1b[?1049l\x1b]0;\x07");
    }
    #[cfg(unix)]
    if let Some(t) = SAVED.get() {
        let fd = TTY.load(Ordering::SeqCst);
        if fd >= 0 {
            unsafe {
                libc::tcsetattr(fd, libc::TCSANOW, t);
            }
        }
    }
}

#[cfg(unix)]
fn out(bytes: &[u8]) {
    let mut rest = bytes;
    while !rest.is_empty() {
        let n = unsafe { libc::write(libc::STDOUT_FILENO, rest.as_ptr().cast(), rest.len()) };
        if n <= 0 {
            if n < 0 && std::io::Error::last_os_error().kind() == std::io::ErrorKind::Interrupted {
                continue;
            }
            return;
        }
        rest = &rest[n as usize..];
    }
}

#[cfg(not(unix))]
fn out(bytes: &[u8]) {
    use std::io::Write;
    let mut o = std::io::stdout();
    let _ = o.write_all(bytes);
    let _ = o.flush();
}

/// crossterm puts the terminal behind stdin in raw mode, or `/dev/tty` when stdin is not one.
#[cfg(unix)]
fn save_termios() {
    if SAVED.get().is_some() {
        return;
    }
    unsafe {
        let fd = if libc::isatty(libc::STDIN_FILENO) == 1 {
            libc::STDIN_FILENO
        } else {
            libc::open(c"/dev/tty".as_ptr(), libc::O_RDWR | libc::O_CLOEXEC)
        };
        if fd < 0 {
            return;
        }
        let mut t = std::mem::MaybeUninit::<libc::termios>::uninit();
        if libc::tcgetattr(fd, t.as_mut_ptr()) == 0 {
            TTY.store(fd, Ordering::SeqCst);
            let _ = SAVED.set(t.assume_init());
        } else if fd != libc::STDIN_FILENO {
            libc::close(fd);
        }
    }
}

fn install() {
    oc_util::panic::before_print(on_panic);
    #[cfg(unix)]
    {
        // SIGPIPE stays ignored (Rust's default); SIGBUS is left to Rust's stack-overflow handler on macOS.
        let mut signals = vec![libc::SIGINT, libc::SIGTERM, libc::SIGQUIT, libc::SIGABRT, libc::SIGHUP];
        if cfg!(not(target_os = "macos")) {
            signals.push(libc::SIGBUS);
        }
        for sig in signals {
            unsafe {
                let mut old: libc::sigaction = std::mem::zeroed();
                if libc::sigaction(sig, std::ptr::null(), &mut old) != 0 || old.sa_sigaction == libc::SIG_IGN {
                    continue;
                }
                let mut act: libc::sigaction = std::mem::zeroed();
                act.sa_sigaction = on_signal as *const () as usize;
                act.sa_flags = libc::SA_RESTART;
                libc::sigemptyset(&mut act.sa_mask);
                libc::sigaction(sig, &act, std::ptr::null_mut());
            }
        }
    }
}

/// Restore, then die of the same signal.
#[cfg(unix)]
extern "C" fn on_signal(sig: libc::c_int) {
    restore_now();
    unsafe {
        libc::signal(sig, libc::SIG_DFL);
        libc::raise(sig);
    }
}

/// A panic on the UI thread ends the process: restore first so the message lands on the normal screen. A panic
/// in a background task does not, and printing it would scribble over the TUI; it is only logged.
fn on_panic(main: bool) -> bool {
    if !armed() {
        return true;
    }
    if main {
        restore_now();
        return true;
    }
    false
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    #[test]
    fn restore_runs_once_and_only_while_armed() {
        MODES.store(MOUSE | PASTE, Ordering::SeqCst);
        assert!(armed());
        assert!(!on_panic(false), "a background panic is not printed over the screen");
        assert!(armed(), "a background panic leaves the screen alone");
        assert!(on_panic(true));
        assert!(!armed());
        assert!(on_panic(false), "printed normally once the terminal is back");
    }
}
