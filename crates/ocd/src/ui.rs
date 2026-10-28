//! `@clack/prompts` on the terminal: intro/outro/log/spinner/confirm with clack's glyphs and line
//! layout; `select`, `multiselect`, `text`, `password`, `autocomplete` and upstream's integration
//! picker through the ported prompt engine in `clack.rs`.

use std::io::Read;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

const S_BAR: &str = "│";

fn color() -> bool {
    crate::term::stdout_is_tty() && std::env::var("NO_COLOR").map(|v| v.is_empty()).unwrap_or(true)
}

fn paint(code: &str, s: &str) -> String {
    if color() { format!("\x1b[{code}m{s}\x1b[39m") } else { s.to_string() }
}

fn gray(s: &str) -> String {
    paint("90", s)
}

/// Where clack output goes (stdout by default, stderr for `session export`).
#[derive(Clone, Copy, PartialEq)]
pub enum Out {
    Stdout,
    Stderr,
}

fn write(out: Out, s: &str) {
    match out {
        Out::Stdout => crate::term::write_stdout(s),
        Out::Stderr => crate::term::write_stderr(s),
    }
}

pub fn intro_to(out: Out, title: &str) {
    write(out, &format!("{}  {title}\n", gray("┌")));
}

pub fn intro(title: &str) {
    intro_to(Out::Stdout, title)
}

pub fn outro_to(out: Out, msg: &str) {
    write(out, &format!("{}\n{}  {msg}\n\n", gray(S_BAR), gray("└")));
}

pub fn outro(msg: &str) {
    outro_to(Out::Stdout, msg)
}

pub fn cancel_to(out: Out, msg: &str) {
    write(out, &format!("{}  {}\n\n", gray("└"), paint("31", msg)));
}

pub fn cancel(msg: &str) {
    cancel_to(Out::Stdout, msg)
}

fn message(out: Out, symbol: &str, msg: &str) {
    let mut s = format!("{}\n", gray(S_BAR));
    for (i, line) in msg.split('\n').enumerate() {
        if i == 0 {
            s.push_str(&format!("{symbol}  {line}\n"));
        } else {
            s.push_str(&format!("{}  {line}\n", gray(S_BAR)));
        }
    }
    write(out, &s);
}

pub mod log {
    use super::*;
    pub fn message(msg: &str) {
        super::message(Out::Stdout, &gray(S_BAR), msg)
    }
    pub fn info(msg: &str) {
        super::message(Out::Stdout, &paint("34", "●"), msg)
    }
    pub fn success(msg: &str) {
        super::message(Out::Stdout, &paint("32", "◆"), msg)
    }
    pub fn step(msg: &str) {
        super::message(Out::Stdout, &paint("32", "◇"), msg)
    }
    pub fn warn(msg: &str) {
        super::message(Out::Stdout, &paint("33", "▲"), msg)
    }
    pub fn error(msg: &str) {
        super::message(Out::Stdout, &paint("31", "■"), msg)
    }
    pub fn error_to(out: Out, msg: &str) {
        super::message(out, &paint("31", "■"), msg)
    }
}

/// `spinner()`
pub struct Spinner {
    running: Arc<AtomicBool>,
    handle: Option<std::thread::JoinHandle<()>>,
    tty: bool,
}

impl Spinner {
    pub fn start(msg: &str) -> Spinner {
        let tty = crate::term::stdout_is_tty();
        crate::term::write_stdout(&format!("{}\n", gray(S_BAR)));
        let running = Arc::new(AtomicBool::new(true));
        let handle = if tty {
            let r = running.clone();
            let msg = msg.to_string();
            Some(std::thread::spawn(move || {
                let frames = ["◒", "◐", "◓", "◑"];
                let mut i = 0;
                while r.load(Ordering::SeqCst) {
                    crate::term::write_stdout(&format!("\r\x1b[2K{}  {msg}", paint("35", frames[i % 4])));
                    i += 1;
                    std::thread::sleep(std::time::Duration::from_millis(80));
                }
            }))
        } else {
            crate::term::write_stdout(&format!("{}  {msg}\n", paint("35", "◒")));
            None
        };
        Spinner { running, handle, tty }
    }

    pub fn stop(mut self, msg: &str, code: i32) {
        self.running.store(false, Ordering::SeqCst);
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
        let symbol = if code == 0 { paint("32", "◇") } else { paint("31", "■") };
        let prefix = if self.tty { "\r\x1b[2K" } else { "" };
        crate::term::write_stdout(&format!("{prefix}{symbol}  {msg}\n"));
    }
}

impl Drop for Spinner {
    fn drop(&mut self) {
        self.running.store(false, Ordering::SeqCst);
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }
}

/// A cancelled prompt (`isCancel`).
#[derive(Debug)]
pub struct Cancelled;

impl std::fmt::Display for Cancelled {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Cancelled")
    }
}

impl std::error::Error for Cancelled {}

pub struct Choice {
    pub value: String,
    pub label: String,
    pub hint: Option<String>,
}

/// clack `confirm({message, initialValue})`.
pub fn confirm(message: &str, initial: bool) -> Result<bool, Cancelled> {
    Ok(run_prompt(crate::clack::Confirm::new(message, initial), None, Out::Stdout)?.value)
}

/// node's `setRawMode(true)` (libuv `UV_TTY_MODE_RAW`), restored on drop.
#[cfg(unix)]
struct RawMode {
    orig: libc::termios,
}

#[cfg(unix)]
impl RawMode {
    fn enter() -> Option<RawMode> {
        unsafe {
            let mut t: libc::termios = std::mem::zeroed();
            if libc::tcgetattr(0, &mut t) != 0 {
                return None;
            }
            let orig = t;
            t.c_iflag &= !(libc::BRKINT | libc::ICRNL | libc::INPCK | libc::ISTRIP | libc::IXON);
            t.c_oflag |= libc::ONLCR;
            t.c_cflag |= libc::CS8;
            t.c_lflag &= !(libc::ECHO | libc::ICANON | libc::IEXTEN | libc::ISIG);
            t.c_cc[libc::VMIN] = 1;
            t.c_cc[libc::VTIME] = 0;
            libc::tcsetattr(0, libc::TCSADRAIN, &t);
            Some(RawMode { orig })
        }
    }
}

#[cfg(unix)]
impl Drop for RawMode {
    fn drop(&mut self) {
        unsafe {
            libc::tcsetattr(0, libc::TCSADRAIN, &self.orig);
        }
    }
}

static SIGNALLED_WINCH: AtomicBool = AtomicBool::new(false);
static SIGNALLED_INT: AtomicBool = AtomicBool::new(false);

#[cfg(unix)]
extern "C" fn on_winch(_: libc::c_int) {
    SIGNALLED_WINCH.store(true, Ordering::SeqCst);
}

#[cfg(unix)]
extern "C" fn on_int(_: libc::c_int) {
    SIGNALLED_INT.store(true, Ordering::SeqCst);
}

/// A signal handler installed without `SA_RESTART` (so a blocked `poll` wakes up), restored on drop.
#[cfg(unix)]
struct Handler {
    signal: libc::c_int,
    old: libc::sigaction,
}

#[cfg(unix)]
impl Handler {
    fn install(signal: libc::c_int, f: extern "C" fn(libc::c_int)) -> Handler {
        unsafe {
            let mut action: libc::sigaction = std::mem::zeroed();
            action.sa_sigaction = f as usize;
            libc::sigemptyset(&mut action.sa_mask);
            let mut old: libc::sigaction = std::mem::zeroed();
            libc::sigaction(signal, &action, &mut old);
            Handler { signal, old }
        }
    }
}

#[cfg(unix)]
impl Drop for Handler {
    fn drop(&mut self) {
        unsafe {
            libc::sigaction(self.signal, &self.old, std::ptr::null_mut());
        }
    }
}

/// `output.rows` of a terminal stream.
fn tty_rows(out: Out) -> Option<usize> {
    match out {
        Out::Stdout => crate::term::rows(),
        Out::Stderr => {
            #[cfg(unix)]
            unsafe {
                if libc::isatty(2) != 1 {
                    return None;
                }
                let mut ws: libc::winsize = std::mem::zeroed();
                if libc::ioctl(2, libc::TIOCGWINSZ, &mut ws) == 0 && ws.ws_row > 0 {
                    return Some(ws.ws_row as usize);
                }
                None
            }
            #[cfg(not(unix))]
            None
        }
    }
}

/// Whether more input arrives within `ms` (readline's `escapeCodeTimeout` is 50 ms).
fn input_within(ms: i32) -> bool {
    #[cfg(unix)]
    unsafe {
        let mut fd = libc::pollfd { fd: 0, events: libc::POLLIN, revents: 0 };
        libc::poll(&mut fd, 1, ms) > 0
    }
    #[cfg(not(unix))]
    {
        let _ = ms;
        true
    }
}

/// Waits for input on stdin; false when a signal interrupted the wait.
fn wait_input() -> bool {
    #[cfg(unix)]
    unsafe {
        let mut fd = libc::pollfd { fd: 0, events: libc::POLLIN, revents: 0 };
        libc::poll(&mut fd, 1, -1) > 0
    }
    #[cfg(not(unix))]
    true
}

/// After readline closed (ctrl+d on an empty line) the input is paused and the terminal back in its
/// normal mode: nothing answers the prompt until ctrl+c interrupts the command.
fn wait_for_interrupt() {
    #[cfg(unix)]
    {
        let _int = Handler::install(libc::SIGINT, on_int);
        SIGNALLED_INT.store(false, Ordering::SeqCst);
        while !SIGNALLED_INT.load(Ordering::SeqCst) {
            unsafe {
                libc::pause();
            }
        }
    }
}

/// Runs a clack prompt on the terminal until it submits or cancels, re-rendering on resize like
/// `output.on("resize", render)`.
fn run_prompt<K: crate::clack::Kind>(kind: K, initial_user_input: Option<&str>, out: Out) -> Result<K, Cancelled> {
    let pc = crate::clack::Pc { on: color() };
    let mut prompt = crate::clack::Prompt::new(kind, pc, crate::term::columns(), tty_rows(out));
    #[cfg(unix)]
    let mut raw = RawMode::enter();
    #[cfg(not(unix))]
    let mut raw: Option<()> = None;
    #[cfg(unix)]
    let _winch = Handler::install(libc::SIGWINCH, on_winch);
    SIGNALLED_WINCH.store(false, Ordering::SeqCst);
    write(out, &prompt.start(initial_user_input));
    let mut stdin = std::io::stdin();
    let mut buf: Vec<u8> = Vec::new();
    while !prompt.done() {
        let ready = wait_input();
        if SIGNALLED_WINCH.swap(false, Ordering::SeqCst) {
            write(out, &prompt.resize(crate::term::columns(), tty_rows(out)));
        }
        if !ready {
            continue;
        }
        let mut chunk = [0u8; 256];
        let n = match stdin.read(&mut chunk) {
            Ok(n) => n,
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(_) => 0,
        };
        if n == 0 {
            // End of input closes readline the same way ctrl+d does.
            drop(raw.take());
            wait_for_interrupt();
            return Err(Cancelled);
        }
        buf.extend_from_slice(&chunk[..n]);
        let mut keys = crate::clack::parse_keys(&mut buf, false);
        if !buf.is_empty() && !input_within(50) {
            keys.extend(crate::clack::parse_keys(&mut buf, true));
        }
        for key in keys {
            if prompt.done() {
                break;
            }
            write(out, &prompt.press(&key));
            if prompt.closed() {
                drop(raw.take());
                wait_for_interrupt();
                return Err(Cancelled);
            }
        }
    }
    if prompt.ctx.state == crate::clack::State::Cancel { Err(Cancelled) } else { Ok(prompt.kind) }
}

fn options(choices: &[Choice]) -> Vec<crate::clack::Opt> {
    choices.iter().map(|c| crate::clack::Opt { value: c.value.clone(), label: c.label.clone(), hint: c.hint.clone() }).collect()
}

/// clack `select({message, options, initialValue})`.
pub fn select(message: &str, choices: &[Choice], initial: Option<&str>) -> Result<String, Cancelled> {
    let kind = run_prompt(crate::clack::Select::new(message, options(choices), initial), None, Out::Stdout)?;
    Ok(kind.value().to_string())
}

/// clack `multiselect({message, options, initialValues, required})`.
pub fn multiselect(message: &str, choices: &[Choice], initial: &[String], required: bool) -> Result<Vec<String>, Cancelled> {
    let pc = crate::clack::Pc { on: color() };
    Ok(run_prompt(crate::clack::MultiSelect::new(message, options(choices), initial, required, pc), None, Out::Stdout)?.value)
}

/// Options of clack's `text` prompt.
#[derive(Default)]
pub struct TextOptions<'a> {
    pub placeholder: Option<&'a str>,
    pub initial_value: Option<&'a str>,
}

/// clack `text({message, placeholder, initialValue, validate})`.
pub fn text(message: &str, opts: TextOptions, validate: &dyn Fn(Option<&str>) -> Option<String>) -> Result<String, Cancelled> {
    let mut kind = crate::clack::Text::new(message, Some(validate));
    kind.placeholder = opts.placeholder.map(String::from);
    Ok(run_prompt(kind, opts.initial_value, Out::Stdout)?.value.unwrap_or_default())
}

/// clack `password({message, validate})`.
pub fn password(message: &str, validate: &dyn Fn(Option<&str>) -> Option<String>) -> Result<String, Cancelled> {
    Ok(run_prompt(crate::clack::Password::new(message, Some(validate)), None, Out::Stdout)?.value.unwrap_or_default())
}

/// clack `autocomplete({message, options, maxItems, output})`: the chosen value, `None` when Enter
/// was pressed with nothing matching (upstream resolves `undefined`).
pub fn autocomplete(out: Out, message: &str, choices: &[Choice], max_items: Option<usize>) -> Result<Option<String>, Cancelled> {
    Ok(run_prompt(crate::clack::Autocomplete::new(message, options(choices), max_items), None, out)?.state.value)
}

pub use crate::clack::IntegrationChoice;

/// Upstream's `selectIntegration(choices, kind)`.
pub fn select_integration(choices: Vec<IntegrationChoice>, kind: &str) -> Result<String, Cancelled> {
    Ok(run_prompt(crate::clack::IntegrationPicker::new(choices, kind), None, Out::Stdout)?.state.value.unwrap_or_default())
}

/// The validator upstream passes to `text`/`password`: `!value ? "Required" : undefined`.
pub fn required(value: Option<&str>) -> Option<String> {
    value.filter(|v| !v.is_empty()).is_none().then(|| "Required".to_string())
}

pub fn interactive() -> bool {
    crate::term::stdin_is_tty() && crate::term::stdout_is_tty()
}

/// Open a URL in the default browser (best effort).
pub fn open_url(url: &str) {
    let cmd = if cfg!(target_os = "macos") { "open" } else if cfg!(windows) { "explorer" } else { "xdg-open" };
    let _ = std::process::Command::new(cmd)
        .arg(url)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn();
}
