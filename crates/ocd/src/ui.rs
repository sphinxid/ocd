//! A small `@clack/prompts`-style terminal UI (intro/outro/log/spinner/select/confirm/text):
//! same glyphs and line layout, simplified interaction (arrow-key select, line-edited text).

use std::io::{Read, Write};
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

#[cfg(unix)]
struct Raw {
    orig: libc::termios,
}

#[cfg(unix)]
impl Raw {
    fn enter(echo: bool) -> Option<Raw> {
        unsafe {
            let mut t: libc::termios = std::mem::zeroed();
            if libc::tcgetattr(0, &mut t) != 0 {
                return None;
            }
            let orig = t;
            t.c_lflag &= !(libc::ICANON | libc::ISIG);
            if !echo {
                t.c_lflag &= !libc::ECHO;
            } else {
                t.c_lflag &= !libc::ECHO;
            }
            t.c_cc[libc::VMIN] = 1;
            t.c_cc[libc::VTIME] = 0;
            libc::tcsetattr(0, libc::TCSANOW, &t);
            Some(Raw { orig })
        }
    }
}

#[cfg(unix)]
impl Drop for Raw {
    fn drop(&mut self) {
        unsafe {
            libc::tcsetattr(0, libc::TCSANOW, &self.orig);
        }
    }
}

enum Key {
    Up,
    Down,
    Enter,
    Cancel,
    Backspace,
    Char(char),
    Other,
}

fn read_key() -> Key {
    let mut b = [0u8; 1];
    let mut stdin = std::io::stdin();
    if stdin.read(&mut b).unwrap_or(0) == 0 {
        return Key::Cancel;
    }
    match b[0] {
        3 | 4 => Key::Cancel,
        b'\r' | b'\n' => Key::Enter,
        127 | 8 => Key::Backspace,
        27 => {
            let mut seq = [0u8; 2];
            if stdin.read(&mut seq[..1]).unwrap_or(0) == 0 {
                return Key::Cancel;
            }
            if seq[0] != b'[' {
                return Key::Cancel;
            }
            let _ = stdin.read(&mut seq[1..2]);
            match seq[1] {
                b'A' => Key::Up,
                b'B' => Key::Down,
                _ => Key::Other,
            }
        }
        c if c >= 0x20 => {
            // utf-8 continuation
            let len = match c {
                0xc0..=0xdf => 2,
                0xe0..=0xef => 3,
                0xf0..=0xf7 => 4,
                _ => 1,
            };
            let mut buf = vec![c];
            for _ in 1..len {
                let mut x = [0u8; 1];
                let _ = stdin.read(&mut x);
                buf.push(x[0]);
            }
            String::from_utf8(buf).ok().and_then(|s| s.chars().next()).map(Key::Char).unwrap_or(Key::Other)
        }
        _ => Key::Other,
    }
}

pub struct Choice {
    pub value: String,
    pub label: String,
    pub hint: Option<String>,
}

/// `select` / `autocomplete` (type to filter).
pub fn select(message: &str, choices: &[Choice], max_items: usize) -> Result<String, Cancelled> {
    #[cfg(unix)]
    let _raw = Raw::enter(false);
    let mut out = std::io::stdout();
    let mut index = 0usize;
    let mut search = String::new();
    let mut drawn = 0usize;
    loop {
        let filtered: Vec<&Choice> = choices
            .iter()
            .filter(|c| search.is_empty() || c.label.to_lowercase().contains(&search.to_lowercase()))
            .collect();
        if index >= filtered.len().max(1) {
            index = filtered.len().saturating_sub(1);
        }
        let mut s = String::new();
        if drawn > 0 {
            s.push_str(&format!("\x1b[{drawn}A\r\x1b[J"));
        }
        let mut lines = vec![format!("{}", gray(S_BAR)), format!("{}  {message}", paint("36", "◆"))];
        if !search.is_empty() {
            lines.push(format!("{}  Search: {search}", paint("36", S_BAR)));
        }
        let start = index.saturating_sub(max_items.saturating_sub(1));
        for (i, c) in filtered.iter().enumerate().skip(start).take(max_items) {
            let hint = c.hint.as_ref().map(|h| format!(" {}", gray(&format!("({h})")))).unwrap_or_default();
            if i == index {
                lines.push(format!("{}  {} {}{hint}", paint("36", S_BAR), paint("32", "●"), c.label));
            } else {
                lines.push(format!("{}  {} {}", paint("36", S_BAR), gray("○"), gray(&c.label)));
            }
        }
        if filtered.is_empty() {
            lines.push(format!("{}  {}", paint("36", S_BAR), gray("No matches")));
        }
        lines.push(paint("36", "└"));
        drawn = lines.len();
        s.push_str(&lines.join("\n"));
        s.push('\n');
        let _ = out.write_all(s.as_bytes());
        let _ = out.flush();
        match read_key() {
            Key::Up => index = index.saturating_sub(1),
            Key::Down => {
                if index + 1 < filtered.len() {
                    index += 1
                }
            }
            Key::Enter => {
                if let Some(c) = filtered.get(index) {
                    let s = format!("\x1b[{drawn}A\r\x1b[J{}\n{}  {message}\n{}  {}\n", gray(S_BAR), paint("32", "◇"), gray(S_BAR), gray(&c.label));
                    let _ = out.write_all(s.as_bytes());
                    let _ = out.flush();
                    return Ok(c.value.clone());
                }
            }
            Key::Cancel => {
                let s = format!("\x1b[{drawn}A\r\x1b[J{}\n{}  {message}\n", gray(S_BAR), paint("31", "■"));
                let _ = out.write_all(s.as_bytes());
                return Err(Cancelled);
            }
            Key::Backspace => {
                search.pop();
            }
            Key::Char(c) => {
                search.push(c);
                index = 0;
            }
            Key::Other => {}
        }
    }
}

/// `confirm({message, initialValue})`
pub fn confirm(message: &str, initial: bool) -> Result<bool, Cancelled> {
    let choices = [
        Choice { value: "yes".into(), label: "Yes".into(), hint: None },
        Choice { value: "no".into(), label: "No".into(), hint: None },
    ];
    let ordered: Vec<Choice> = if initial {
        choices.into_iter().collect()
    } else {
        let [a, b] = choices;
        vec![b, a]
    };
    select(message, &ordered, 2).map(|v| v == "yes")
}

/// `text` / `password` prompt; `validate` returns an error message to re-prompt.
pub fn text(message: &str, secret: bool, validate: &dyn Fn(&str) -> Option<String>) -> Result<String, Cancelled> {
    #[cfg(unix)]
    let _raw = Raw::enter(false);
    let mut out = std::io::stdout();
    let mut value = String::new();
    let mut error: Option<String> = None;
    let mut drawn = 0usize;
    loop {
        let shown = if secret { "▪".repeat(value.chars().count()) } else { value.clone() };
        let mut lines = vec![gray(S_BAR), format!("{}  {message}", paint("36", "◆")), format!("{}  {shown}", paint("36", S_BAR))];
        match &error {
            Some(e) => lines.push(format!("{}  {}", paint("33", "└"), paint("33", e))),
            None => lines.push(paint("36", "└")),
        }
        let mut s = String::new();
        if drawn > 0 {
            s.push_str(&format!("\x1b[{drawn}A\r\x1b[J"));
        }
        drawn = lines.len();
        s.push_str(&lines.join("\n"));
        s.push('\n');
        let _ = out.write_all(s.as_bytes());
        let _ = out.flush();
        match read_key() {
            Key::Enter => match validate(&value) {
                Some(e) => error = Some(e),
                None => {
                    let shown = if secret { "▪".repeat(value.chars().count()) } else { value.clone() };
                    let s = format!("\x1b[{drawn}A\r\x1b[J{}\n{}  {message}\n{}  {}\n", gray(S_BAR), paint("32", "◇"), gray(S_BAR), gray(&shown));
                    let _ = out.write_all(s.as_bytes());
                    let _ = out.flush();
                    return Ok(value);
                }
            },
            Key::Cancel => {
                let s = format!("\x1b[{drawn}A\r\x1b[J{}\n{}  {message}\n", gray(S_BAR), paint("31", "■"));
                let _ = out.write_all(s.as_bytes());
                return Err(Cancelled);
            }
            Key::Backspace => {
                value.pop();
                error = None;
            }
            Key::Char(c) => {
                value.push(c);
                error = None;
            }
            _ => {}
        }
    }
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
