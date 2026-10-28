//! Port of the `@clack/core` / `@clack/prompts` 1.0.0-alpha.1 prompt machinery used by the auth
//! flows: the `Prompt` keypress and render loop (`core/src/prompts/prompt.ts`: frames hard-wrapped
//! with `wrap-ansi` 8.1.0, redrawn from the first changed line, cursor hidden while active), the
//! `select`, `multiselect`, `text`, `password` and `autocomplete` prompts with their renderers and
//! `limitOptions`, upstream's integration picker (`cli/src/ui/integration-picker.ts`),
//! the node `readline` line editing that `text`/`password` sit on, and picocolors 1.1.1.
//!
//! Everything here is pure: prompts take parsed keypresses and return the bytes to write, so the
//! frames can be compared byte for byte with recorded clack output. `ui.rs` drives them on a TTY.

use std::sync::LazyLock;

// ---------------------------------------------------------------------------------------------
// picocolors 1.1.1

/// A picocolors instance (`createColors(enabled)`).
#[derive(Clone, Copy)]
pub struct Pc {
    pub on: bool,
}

impl Pc {
    fn f(&self, open: &str, close: &str, replace: &str, input: &str) -> String {
        if !self.on {
            return input.to_string();
        }
        // `string.indexOf(close, open.length)`, then every later occurrence replaced.
        let start = open.len().min(input.len());
        let Some(first) = input.get(start..).and_then(|rest| rest.find(close)).map(|i| i + start) else {
            return format!("{open}{input}{close}");
        };
        let mut out = String::from(open);
        let mut cursor = 0;
        let mut index = Some(first);
        while let Some(i) = index {
            out.push_str(&input[cursor..i]);
            out.push_str(replace);
            cursor = i + close.len();
            index = input[cursor..].find(close).map(|j| j + cursor);
        }
        out.push_str(&input[cursor..]);
        out.push_str(close);
        out
    }
    pub fn reset(&self, s: &str) -> String {
        self.f("\x1b[0m", "\x1b[0m", "\x1b[0m", s)
    }
    pub fn bold(&self, s: &str) -> String {
        self.f("\x1b[1m", "\x1b[22m", "\x1b[22m\x1b[1m", s)
    }
    pub fn dim(&self, s: &str) -> String {
        self.f("\x1b[2m", "\x1b[22m", "\x1b[22m\x1b[2m", s)
    }
    pub fn inverse(&self, s: &str) -> String {
        self.f("\x1b[7m", "\x1b[27m", "\x1b[7m", s)
    }
    pub fn hidden(&self, s: &str) -> String {
        self.f("\x1b[8m", "\x1b[28m", "\x1b[8m", s)
    }
    pub fn strikethrough(&self, s: &str) -> String {
        self.f("\x1b[9m", "\x1b[29m", "\x1b[9m", s)
    }
    pub fn red(&self, s: &str) -> String {
        self.f("\x1b[31m", "\x1b[39m", "\x1b[31m", s)
    }
    pub fn green(&self, s: &str) -> String {
        self.f("\x1b[32m", "\x1b[39m", "\x1b[32m", s)
    }
    pub fn yellow(&self, s: &str) -> String {
        self.f("\x1b[33m", "\x1b[39m", "\x1b[33m", s)
    }
    pub fn cyan(&self, s: &str) -> String {
        self.f("\x1b[36m", "\x1b[39m", "\x1b[36m", s)
    }
    pub fn gray(&self, s: &str) -> String {
        self.f("\x1b[90m", "\x1b[39m", "\x1b[90m", s)
    }
    pub fn bg_white(&self, s: &str) -> String {
        self.f("\x1b[47m", "\x1b[49m", "\x1b[47m", s)
    }
}

// ---------------------------------------------------------------------------------------------
// glyphs (`prompts/src/common.ts`, unicode terminal)

pub const S_BAR: &str = "│";
pub const S_BAR_END: &str = "└";
const S_STEP_ACTIVE: &str = "◆";
const S_STEP_CANCEL: &str = "■";
const S_STEP_ERROR: &str = "▲";
const S_STEP_SUBMIT: &str = "◇";
const S_RADIO_ACTIVE: &str = "●";
const S_RADIO_INACTIVE: &str = "○";
const S_CHECKBOX_ACTIVE: &str = "◻";
const S_CHECKBOX_SELECTED: &str = "◼";
const S_CHECKBOX_INACTIVE: &str = "◻";
const S_PASSWORD_MASK: &str = "▪";

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum State {
    Initial,
    Active,
    Error,
    Submit,
    Cancel,
}

fn symbol(pc: Pc, state: State) -> String {
    match state {
        State::Initial | State::Active => pc.cyan(S_STEP_ACTIVE),
        State::Cancel => pc.red(S_STEP_CANCEL),
        State::Error => pc.yellow(S_STEP_ERROR),
        State::Submit => pc.green(S_STEP_SUBMIT),
    }
}

// ---------------------------------------------------------------------------------------------
// string-width 5.1.2 / strip-ansi 7.1.0 / wrap-ansi 8.1.0

static ANSI: LazyLock<regex::Regex> = LazyLock::new(|| {
    regex::Regex::new(
        r"[\x1b\x{9b}][\[\]()#;?]*(?:(?:(?:(?:;[-a-zA-Z\d/#&.:=?%@~_]+)*|[a-zA-Z\d]+(?:;[-a-zA-Z\d/#&.:=?%@~_]*)*)?\x07)|(?:(?:\d{1,4}(?:;\d{0,4})*)?[\dA-PR-TZcf-ntqry=><~]))",
    )
    .unwrap()
});

fn strip_ansi(s: &str) -> String {
    ANSI.replace_all(s, "").into_owned()
}

/// `stringWidth(s)` (ambiguous characters narrow).
pub fn string_width(s: &str) -> usize {
    use unicode_width::UnicodeWidthChar;
    strip_ansi(s)
        .chars()
        .map(|c| {
            let cp = c as u32;
            if cp <= 0x1f || (0x7f..=0x9f).contains(&cp) || (0x300..=0x36f).contains(&cp) {
                0
            } else if c.width() == Some(2) {
                2
            } else {
                1
            }
        })
        .sum()
}

const ESC: char = '\x1b';
const CSI8: char = '\u{9b}';

fn wrap_word(rows: &mut Vec<String>, word: &str, columns: usize) {
    let chars: Vec<char> = word.chars().collect();
    let mut inside_escape = false;
    let mut inside_link = false;
    let mut visible = string_width(&strip_ansi(rows.last().unwrap()));
    for (index, &ch) in chars.iter().enumerate() {
        let len = string_width(ch.encode_utf8(&mut [0; 4]));
        if visible + len <= columns {
            rows.last_mut().unwrap().push(ch);
        } else {
            rows.push(ch.to_string());
            visible = 0;
        }
        if ch == ESC || ch == CSI8 {
            inside_escape = true;
            inside_link = chars[index + 1..].iter().collect::<String>().starts_with("]8;;");
        }
        if inside_escape {
            if inside_link {
                if ch == '\x07' {
                    inside_escape = false;
                    inside_link = false;
                }
            } else if ch == 'm' {
                inside_escape = false;
            }
            continue;
        }
        visible += len;
        if visible == columns && index < chars.len() - 1 {
            rows.push(String::new());
            visible = 0;
        }
    }
    if visible == 0 && !rows.last().unwrap().is_empty() && rows.len() > 1 {
        let last = rows.pop().unwrap();
        rows.last_mut().unwrap().push_str(&last);
    }
}

fn trim_spaces_right(s: &str) -> String {
    let words: Vec<&str> = s.split(' ').collect();
    let mut last = words.len();
    while last > 0 && string_width(words[last - 1]) == 0 {
        last -= 1;
    }
    if last == words.len() {
        return s.to_string();
    }
    words[..last].join(" ") + &words[last..].concat()
}

/// ansi-styles' open → close code map.
fn close_code(code: u32) -> Option<u32> {
    Some(match code {
        0 => 0,
        1 | 2 => 22,
        3 => 23,
        4 => 24,
        53 => 55,
        7 => 27,
        8 => 28,
        9 => 29,
        30..=37 | 90..=97 => 39,
        40..=47 | 100..=107 => 49,
        _ => return None,
    })
}

static ESCAPE_CODE: LazyLock<regex::Regex> = LazyLock::new(|| regex::Regex::new(r"(?:\[(?<code>\d+)m|\]8;;(?<uri>.*)\x07)").unwrap());

/// One line of `wrapAnsi(line, columns, {hard: true})`. `columns: None` is
/// `process.stdout.columns` being undefined: nothing wraps, but lines are still trimmed.
fn wrap_line(line: &str, columns: Option<usize>) -> String {
    if line.trim().is_empty() {
        return String::new();
    }
    let gt = |a: usize| columns.is_some_and(|c| a > c);
    let words: Vec<&str> = line.split(' ').collect();
    let lengths: Vec<usize> = words.iter().map(|w| string_width(w)).collect();
    let mut rows: Vec<String> = vec![String::new()];
    for (index, word) in words.iter().enumerate() {
        let trimmed = rows.last().unwrap().trim_start().to_string();
        *rows.last_mut().unwrap() = trimmed;
        let mut row_length = string_width(rows.last().unwrap());
        if index != 0 && row_length > 0 {
            rows.last_mut().unwrap().push(' ');
            row_length += 1;
        }
        if let Some(c) = columns
            && lengths[index] > c
        {
            let remaining = c as i64 - row_length as i64;
            let starting_this = 1 + (lengths[index] as i64 - remaining - 1).div_euclid(c as i64);
            let starting_next = (lengths[index] as i64 - 1).div_euclid(c as i64);
            if starting_next < starting_this {
                rows.push(String::new());
            }
            wrap_word(&mut rows, word, c);
            continue;
        }
        if gt(row_length + lengths[index]) && row_length > 0 && lengths[index] > 0 {
            rows.push(String::new());
        }
        rows.last_mut().unwrap().push_str(word);
    }
    let rows: Vec<String> = rows.iter().map(|r| trim_spaces_right(r)).collect();
    let pre: Vec<char> = rows.join("\n").chars().collect();
    let mut out = String::new();
    let mut escape_code: Option<u32> = None;
    let mut escape_url: Option<String> = None;
    for (index, &ch) in pre.iter().enumerate() {
        out.push(ch);
        if ch == ESC || ch == CSI8 {
            let rest: String = pre[index..].iter().collect();
            if let Some(caps) = ESCAPE_CODE.captures(&rest) {
                if let Some(code) = caps.name("code") {
                    let code: u32 = code.as_str().parse().unwrap_or(0);
                    escape_code = if code == 39 { None } else { Some(code) };
                } else if let Some(uri) = caps.name("uri") {
                    escape_url = (!uri.as_str().is_empty()).then(|| uri.as_str().to_string());
                }
            }
        }
        let code = escape_code.and_then(close_code);
        if pre.get(index + 1) == Some(&'\n') {
            if escape_url.is_some() {
                out.push_str("\x1b]8;;\x07");
            }
            if let (Some(e), Some(c)) = (escape_code, code)
                && e != 0
            {
                out.push_str(&format!("\x1b[{c}m"));
            }
        } else if ch == '\n' {
            if let (Some(e), Some(_)) = (escape_code, code)
                && e != 0
            {
                out.push_str(&format!("\x1b[{e}m"));
            }
            if let Some(u) = &escape_url {
                out.push_str(&format!("\x1b]8;;{u}\x07"));
            }
        }
    }
    out
}

/// `wrapAnsi(s, columns, {hard: true})` (the string NFC-normalised first).
pub fn wrap(s: &str, columns: Option<usize>) -> String {
    use unicode_normalization::UnicodeNormalization;
    s.nfc().collect::<String>().replace("\r\n", "\n").split('\n').map(|l| wrap_line(l, columns)).collect::<Vec<_>>().join("\n")
}

// ---------------------------------------------------------------------------------------------
// keypresses (node `readline.emitKeypressEvents`)

/// A node keypress: `char` is the string argument (absent for escape sequences), `name` the key name.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Key {
    pub ch: Option<String>,
    pub name: Option<String>,
    pub ctrl: bool,
    pub meta: bool,
    pub shift: bool,
    pub sequence: String,
}

impl Key {
    fn named(name: &str, ch: Option<&str>, sequence: &str) -> Key {
        Key { ch: ch.map(String::from), name: Some(name.into()), sequence: sequence.into(), ..Default::default() }
    }
}

/// Splits raw terminal input into keypresses. `complete` says no more bytes follow right away (the
/// 50 ms `escapeCodeTimeout` passed), so a trailing lone ESC is the escape key; otherwise an
/// unfinished escape sequence stays in `buf`.
pub fn parse_keys(buf: &mut Vec<u8>, complete: bool) -> Vec<Key> {
    let mut keys = vec![];
    let mut i = 0;
    while i < buf.len() {
        let b = buf[i];
        if b == 0x1b {
            if i + 1 >= buf.len() {
                if !complete {
                    break;
                }
                keys.push(Key { name: Some("escape".into()), meta: false, sequence: "\x1b".into(), ..Default::default() });
                i += 1;
                continue;
            }
            let next = buf[i + 1];
            if next == b'[' || next == b'O' {
                // CSI / SS3: parameters then a final byte.
                let mut j = i + 2;
                while j < buf.len() && (buf[j].is_ascii_digit() || buf[j] == b';') {
                    j += 1;
                }
                if j >= buf.len() {
                    if !complete {
                        break;
                    }
                    i = j;
                    continue;
                }
                let params = String::from_utf8_lossy(&buf[i + 2..j]).to_string();
                let fin = buf[j] as char;
                let sequence = String::from_utf8_lossy(&buf[i..=j]).to_string();
                let mut parts = params.split(';');
                let first = parts.next().unwrap_or("");
                let modifier: u32 = parts.next().and_then(|m| m.parse().ok()).unwrap_or(1).max(1) - 1;
                let name = match (fin, first) {
                    ('A', _) => "up",
                    ('B', _) => "down",
                    ('C', _) => "right",
                    ('D', _) => "left",
                    ('H', _) => "home",
                    ('F', _) => "end",
                    ('Z', _) => "tab",
                    ('~', "1" | "7") => "home",
                    ('~', "4" | "8") => "end",
                    ('~', "2") => "insert",
                    ('~', "3") => "delete",
                    ('~', "5") => "pageup",
                    ('~', "6") => "pagedown",
                    _ => "undefined",
                };
                let mut key = Key { name: (name != "undefined").then(|| name.to_string()), sequence, ..Default::default() };
                key.shift = modifier & 1 != 0 || fin == 'Z';
                key.meta = modifier & 10 != 0;
                key.ctrl = modifier & 4 != 0;
                keys.push(key);
                i = j + 1;
                continue;
            }
            // ESC + key: that key with meta and no string.
            let len = utf8_len(next).min(buf.len() - i - 1);
            let mut sub = buf[i + 1..i + 1 + len].to_vec();
            if let Some(mut k) = parse_keys(&mut sub, true).into_iter().next() {
                k.meta = true;
                k.sequence = format!("\x1b{}", k.sequence);
                k.ch = None;
                keys.push(k);
            }
            i += 1 + len;
            continue;
        }
        let len = utf8_len(b);
        if i + len > buf.len() {
            if !complete {
                break;
            }
            i = buf.len();
            continue;
        }
        let s = String::from_utf8_lossy(&buf[i..i + len]).to_string();
        i += len;
        let key = match b {
            b'\r' => Key::named("return", Some(&s), &s),
            b'\n' => Key::named("enter", Some(&s), &s),
            b'\t' => Key::named("tab", Some(&s), &s),
            0x08 | 0x7f => Key::named("backspace", Some(&s), &s),
            b' ' => Key::named("space", Some(&s), &s),
            1..=26 => Key { ctrl: true, ..Key::named(&((b + b'a' - 1) as char).to_string(), Some(&s), &s) },
            b'0'..=b'9' | b'a'..=b'z' => Key::named(&s, Some(&s), &s),
            b'A'..=b'Z' => Key { shift: true, ..Key::named(&s.to_lowercase(), Some(&s), &s) },
            _ => Key { ch: Some(s.clone()), sequence: s, ..Default::default() },
        };
        keys.push(key);
    }
    buf.drain(..i.min(buf.len()));
    keys
}

fn utf8_len(b: u8) -> usize {
    match b {
        0xc0..=0xdf => 2,
        0xe0..=0xef => 3,
        0xf0..=0xf7 => 4,
        _ => 1,
    }
}

// ---------------------------------------------------------------------------------------------
// readline line editing (the parts `text`/`password` reach)

#[derive(Default, Clone)]
struct Line {
    chars: Vec<char>,
    cursor: usize,
}

impl Line {
    fn text(&self) -> String {
        self.chars.iter().collect()
    }
    fn insert(&mut self, s: &str) {
        for c in s.chars() {
            self.chars.insert(self.cursor, c);
            self.cursor += 1;
        }
    }
    fn clear(&mut self) {
        self.chars.clear();
        self.cursor = 0;
    }
    fn delete_left(&mut self) {
        if self.cursor > 0 && !self.chars.is_empty() {
            self.cursor -= 1;
            self.chars.remove(self.cursor);
        }
    }
    fn delete_right(&mut self) {
        if self.cursor < self.chars.len() {
            self.chars.remove(self.cursor);
        }
    }
    fn word_left(&self) -> usize {
        // `/^\s*(?:[^\w\s]+|\w+)?/` on the reversed text before the cursor.
        let is_word = |c: char| c.is_alphanumeric() || c == '_';
        let mut i = self.cursor;
        while i > 0 && self.chars[i - 1].is_whitespace() {
            i -= 1;
        }
        if i > 0 {
            let word = is_word(self.chars[i - 1]);
            while i > 0 && !self.chars[i - 1].is_whitespace() && is_word(self.chars[i - 1]) == word {
                i -= 1;
            }
        }
        i
    }
    fn word_right(&self) -> usize {
        let is_word = |c: char| c.is_alphanumeric() || c == '_';
        let mut i = self.cursor;
        while i < self.chars.len() && self.chars[i].is_whitespace() {
            i += 1;
        }
        if i < self.chars.len() {
            let word = is_word(self.chars[i]);
            while i < self.chars.len() && !self.chars[i].is_whitespace() && is_word(self.chars[i]) == word {
                i += 1;
            }
        }
        i
    }

    /// `Interface._ttyWrite(s, key)` for an interface with no output, history or completer. Returns
    /// true when the key closes the interface (ctrl+d on an empty line).
    fn key(&mut self, key: &Key) -> bool {
        let name = key.name.as_deref().unwrap_or("");
        if key.ctrl && key.shift {
            match name {
                "backspace" => {
                    self.chars.drain(..self.cursor);
                    self.cursor = 0;
                }
                "delete" => {
                    self.chars.truncate(self.cursor);
                }
                _ => {}
            }
            return false;
        }
        if key.ctrl {
            match name {
                "h" => self.delete_left(),
                "d" if self.cursor == 0 && self.chars.is_empty() => return true,
                "d" => self.delete_right(),
                "u" => {
                    self.chars.drain(..self.cursor);
                    self.cursor = 0;
                }
                "k" => self.chars.truncate(self.cursor),
                "a" => self.cursor = 0,
                "e" => self.cursor = self.chars.len(),
                "b" => self.cursor = self.cursor.saturating_sub(1),
                "f" => self.cursor = (self.cursor + 1).min(self.chars.len()),
                "w" | "backspace" => {
                    let to = self.word_left();
                    self.chars.drain(to..self.cursor);
                    self.cursor = to;
                }
                "delete" => {
                    let to = self.word_right();
                    self.chars.drain(self.cursor..to);
                }
                "left" => self.cursor = self.word_left(),
                "right" => self.cursor = self.word_right(),
                _ => {}
            }
            return false;
        }
        if key.meta {
            match name {
                "b" => self.cursor = self.word_left(),
                "f" => self.cursor = self.word_right(),
                "d" | "delete" => {
                    let to = self.word_right();
                    self.chars.drain(self.cursor..to);
                }
                "backspace" => {
                    let to = self.word_left();
                    self.chars.drain(to..self.cursor);
                    self.cursor = to;
                }
                _ => {}
            }
            return false;
        }
        match name {
            "return" | "enter" => self.clear(),
            "backspace" => self.delete_left(),
            "delete" => self.delete_right(),
            "left" => self.cursor = self.cursor.saturating_sub(1),
            "right" => self.cursor = (self.cursor + 1).min(self.chars.len()),
            "home" => self.cursor = 0,
            "end" => self.cursor = self.chars.len(),
            "up" | "down" | "escape" => {}
            _ => {
                if let Some(s) = key.ch.as_deref().filter(|s| !s.is_empty()) {
                    self.insert(&s.replace('\r', ""));
                }
            }
        }
        false
    }
}

// ---------------------------------------------------------------------------------------------
// Prompt (`core/src/prompts/prompt.ts`)

/// What each prompt kind adds to `Prompt`.
pub trait Kind {
    /// `trackValue`: whether readline's line is the prompt's user input.
    fn track(&self) -> bool {
        false
    }
    fn on_cursor(&mut self, _action: &str) {}
    /// The `key` event: the lower-cased string and the key.
    fn on_key(&mut self, _ch: Option<&str>, _key: &Key) {}
    fn on_user_input(&mut self, _input: &str) {}
    /// The `confirm` event (`y`/`n` typed); true when the prompt handles it by submitting.
    fn on_confirm(&mut self, _yes: bool) -> bool {
        false
    }
    fn validate(&self) -> Option<String> {
        None
    }
    fn finalize(&mut self) {}
    fn render(&self, p: &Ctx) -> String;
}

/// The `Prompt` fields a renderer reads.
pub struct Ctx {
    pub pc: Pc,
    pub state: State,
    pub error: String,
    pub user_input: String,
    pub cursor: usize,
    /// `output.rows` when the output is a terminal (`limitOptions` uses 10 otherwise).
    pub rows: Option<usize>,
    /// `process.stdout.columns`.
    pub columns: Option<usize>,
}

pub struct Prompt<K: Kind> {
    pub kind: K,
    pub ctx: Ctx,
    line: Line,
    /// readline closed (ctrl+d on an empty line): it paused the input and left raw mode, so no
    /// further keypress reaches the prompt.
    closed: bool,
    prev_frame: String,
}

const CURSOR_HIDE: &str = "\x1b[?25l";
const CURSOR_SHOW: &str = "\x1b[?25h";

/// sisteransi `cursor.move(x, y)`.
fn move_cursor(x: i64, y: i64) -> String {
    let mut s = String::new();
    if x < 0 {
        s.push_str(&format!("\x1b[{}D", -x));
    } else if x > 0 {
        s.push_str(&format!("\x1b[{x}C"));
    }
    if y < 0 {
        s.push_str(&format!("\x1b[{}A", -y));
    } else if y > 0 {
        s.push_str(&format!("\x1b[{y}B"));
    }
    s
}

impl<K: Kind> Prompt<K> {
    pub fn new(kind: K, pc: Pc, columns: Option<usize>, rows: Option<usize>) -> Self {
        Prompt {
            kind,
            ctx: Ctx { pc, state: State::Initial, error: String::new(), user_input: String::new(), cursor: 0, rows, columns },
            line: Line::default(),
            closed: false,
            prev_frame: String::new(),
        }
    }

    pub fn closed(&self) -> bool {
        self.closed
    }

    /// The output's `resize` event: render again at the new size.
    pub fn resize(&mut self, columns: Option<usize>, rows: Option<usize>) -> String {
        self.ctx.columns = columns;
        self.ctx.rows = rows;
        self.render()
    }

    pub fn done(&self) -> bool {
        matches!(self.ctx.state, State::Submit | State::Cancel)
    }

    fn set_user_input(&mut self, value: &str, write: bool) {
        self.ctx.user_input = value.to_string();
        self.kind.on_user_input(value);
        if write && self.kind.track() {
            self.line.insert(value);
            self.ctx.cursor = self.line.cursor;
        }
    }

    /// `prompt()` up to the first frame; `initial_user_input` is `initialUserInput`.
    pub fn start(&mut self, initial_user_input: Option<&str>) -> String {
        if let Some(v) = initial_user_input {
            self.set_user_input(v, true);
        }
        self.render()
    }

    /// `onKeypress(char, key)`: the bytes to write (the closing newline and cursor-show included).
    pub fn press(&mut self, key: &Key) -> String {
        let name = key.name.as_deref();
        // readline's own keypress listener runs first, for every prompt.
        if self.line.key(key) {
            self.closed = true;
        }
        if self.kind.track() && name != Some("return") {
            if name.is_some() && key.ch.as_deref() == Some("\t") {
                self.line.delete_left();
            }
            self.ctx.cursor = self.line.cursor;
            let text = self.line.text();
            self.set_user_input(&text, false);
        }
        if self.ctx.state == State::Error {
            self.ctx.state = State::Active;
        }
        if let Some(n) = name {
            if !self.kind.track() {
                let alias = match n {
                    "k" => Some("up"),
                    "j" => Some("down"),
                    "h" => Some("left"),
                    "l" => Some("right"),
                    "\x03" | "escape" => Some("cancel"),
                    _ => None,
                };
                if let Some(a) = alias {
                    self.kind.on_cursor(a);
                }
            }
            if matches!(n, "up" | "down" | "left" | "right" | "space" | "enter" | "cancel") {
                self.kind.on_cursor(n);
            }
        }
        let lower = key.ch.as_deref().map(str::to_lowercase);
        let mut out = String::new();
        // `confirm` submits from its listener: it moves up a line, closes (newline, cursor shown) and
        // unsubscribes everything, so the rest of this keypress only renders and closes again.
        let mut unsubscribed = false;
        if let Some(c @ ("y" | "n")) = lower.as_deref()
            && self.kind.on_confirm(c == "y")
        {
            out.push_str(&move_cursor(0, -1));
            self.ctx.state = State::Submit;
            out.push('\n');
            out.push_str(CURSOR_SHOW);
            unsubscribed = true;
        }
        if !unsubscribed {
            self.kind.on_key(lower.as_deref(), key);
        }
        if name == Some("return") {
            if let Some(problem) = self.kind.validate() {
                self.ctx.error = problem;
                self.ctx.state = State::Error;
                let input = self.ctx.user_input.clone();
                self.line.insert(&input);
            }
            if self.ctx.state != State::Error {
                self.ctx.state = State::Submit;
            }
        }
        let cancel = |v: &str| v == "\x03" || v == "escape";
        if key.ch.as_deref().is_some_and(cancel) || name.is_some_and(cancel) || cancel(&key.sequence) {
            self.ctx.state = State::Cancel;
        }
        if self.done() && !unsubscribed {
            self.kind.finalize();
        }
        out.push_str(&self.render());
        if self.done() {
            out.push('\n');
            if !unsubscribed {
                out.push_str(CURSOR_SHOW);
            }
        }
        out
    }

    fn render(&mut self) -> String {
        let frame = wrap(&self.kind.render(&self.ctx), self.ctx.columns);
        if frame == self.prev_frame {
            return String::new();
        }
        let mut out = String::new();
        if self.ctx.state == State::Initial {
            out.push_str(CURSOR_HIDE);
        } else {
            let prev: Vec<&str> = self.prev_frame.split('\n').collect();
            let next: Vec<&str> = frame.split('\n').collect();
            let diff: Vec<usize> = (0..prev.len().max(next.len())).filter(|&i| prev.get(i) != next.get(i)).collect();
            // `restoreCursor`: the previous frame's height at the current width.
            let height = wrap(&self.prev_frame, self.ctx.columns).split('\n').count();
            out.push_str(&move_cursor(-999, -(height as i64 - 1)));
            if diff.len() == 1 {
                let line = diff[0];
                out.push_str(&move_cursor(0, line as i64));
                out.push_str("\x1b[2K\x1b[G");
                out.push_str(next.get(line).copied().unwrap_or(""));
                out.push_str(&move_cursor(0, next.len() as i64 - line as i64 - 1));
                self.prev_frame = frame;
                return out;
            }
            if diff.len() > 1 {
                let line = diff[0];
                out.push_str(&move_cursor(0, line as i64));
                out.push_str("\x1b[J");
                out.push_str(&next[line.min(next.len())..].join("\n"));
                self.prev_frame = frame;
                return out;
            }
            out.push_str("\x1b[J");
        }
        out.push_str(&frame);
        if self.ctx.state == State::Initial {
            self.ctx.state = State::Active;
        }
        self.prev_frame = frame;
        out
    }
}

// ---------------------------------------------------------------------------------------------
// options (`prompts/src/select.ts` `Option`, `limit-options.ts`)

#[derive(Clone, Debug)]
pub struct Opt {
    pub value: String,
    pub label: String,
    pub hint: Option<String>,
}

fn limit_options(ctx: &Ctx, options: &[Opt], cursor: usize, max_items: Option<usize>, style: &dyn Fn(&Opt, bool) -> String) -> Vec<String> {
    let rows = ctx.rows.unwrap_or(10);
    let output_max = rows.saturating_sub(4);
    let max_items = output_max.min(max_items.unwrap_or(usize::MAX).max(5));
    let len = options.len();
    let mut window = 0usize;
    if cursor + 3 >= window + max_items {
        window = (cursor as i64 - max_items as i64 + 3).min(len as i64 - max_items as i64).max(0) as usize;
    } else if cursor < window + 2 {
        window = cursor.saturating_sub(2);
    }
    let top = max_items < len && window > 0;
    let bottom = max_items < len && window + max_items < len;
    let shown = &options[window.min(len)..(window + max_items).min(len)];
    shown
        .iter()
        .enumerate()
        .map(|(i, o)| if (i == 0 && top) || (i == shown.len() - 1 && bottom) { ctx.pc.dim("...") } else { style(o, i + window == cursor) })
        .collect()
}

fn hint(pc: Pc, o: &Opt) -> String {
    o.hint.as_deref().map(|h| pc.dim(&format!("({h})"))).unwrap_or_default()
}

// ---------------------------------------------------------------------------------------------
// select (`core/src/prompts/select.ts`, `prompts/src/select.ts`)

pub struct Select {
    pub message: String,
    pub options: Vec<Opt>,
    pub max_items: Option<usize>,
    pub cursor: usize,
}

impl Select {
    pub fn new(message: &str, options: Vec<Opt>, initial: Option<&str>) -> Select {
        let cursor = options.iter().position(|o| Some(o.value.as_str()) == initial).unwrap_or(0);
        Select { message: message.into(), options, max_items: None, cursor }
    }
    pub fn value(&self) -> &str {
        &self.options[self.cursor].value
    }
}

impl Kind for Select {
    fn on_cursor(&mut self, action: &str) {
        let last = self.options.len().saturating_sub(1);
        match action {
            "left" | "up" => self.cursor = if self.cursor == 0 { last } else { self.cursor - 1 },
            "down" | "right" => self.cursor = if self.cursor == last { 0 } else { self.cursor + 1 },
            _ => {}
        }
    }
    fn render(&self, p: &Ctx) -> String {
        let pc = p.pc;
        let title = format!("{}\n{}  {}\n", pc.gray(S_BAR), symbol(pc, p.state), self.message);
        let opt = &self.options[self.cursor];
        match p.state {
            State::Submit => format!("{title}{}  {}", pc.gray(S_BAR), pc.dim(&opt.label)),
            State::Cancel => format!("{title}{}  {}\n{}", pc.gray(S_BAR), pc.strikethrough(&pc.dim(&opt.label)), pc.gray(S_BAR)),
            _ => {
                let style = |o: &Opt, active: bool| {
                    if active { format!("{} {} {}", pc.green(S_RADIO_ACTIVE), o.label, hint(pc, o)) } else { format!("{} {}", pc.dim(S_RADIO_INACTIVE), pc.dim(&o.label)) }
                };
                let bar = pc.cyan(S_BAR);
                format!("{title}{bar}  {}\n{}\n", limit_options(p, &self.options, self.cursor, self.max_items, &style).join(&format!("\n{bar}  ")), pc.cyan(S_BAR_END))
            }
        }
    }
}

// ---------------------------------------------------------------------------------------------
// multiselect (`core/src/prompts/multi-select.ts`, `prompts/src/multi-select.ts`)

pub struct MultiSelect {
    pub message: String,
    pub options: Vec<Opt>,
    pub value: Vec<String>,
    pub required: bool,
    pub max_items: Option<usize>,
    pub cursor: usize,
    pc: Pc,
}

impl MultiSelect {
    pub fn new(message: &str, options: Vec<Opt>, initial: &[String], required: bool, pc: Pc) -> MultiSelect {
        MultiSelect { message: message.into(), options, value: initial.to_vec(), required, max_items: None, cursor: 0, pc }
    }

    fn opt(&self, o: &Opt, state: &str) -> String {
        let pc = self.pc;
        match state {
            "active" => format!("{} {} {}", pc.cyan(S_CHECKBOX_ACTIVE), o.label, hint(pc, o)),
            "selected" => format!("{} {} {}", pc.green(S_CHECKBOX_SELECTED), pc.dim(&o.label), hint(pc, o)),
            "cancelled" => pc.strikethrough(&pc.dim(&o.label)),
            "active-selected" => format!("{} {} {}", pc.green(S_CHECKBOX_SELECTED), o.label, hint(pc, o)),
            "submitted" => pc.dim(&o.label),
            _ => format!("{} {}", pc.dim(S_CHECKBOX_INACTIVE), pc.dim(&o.label)),
        }
    }

    fn chosen(&self, state: &str) -> Vec<String> {
        self.options.iter().filter(|o| self.value.contains(&o.value)).map(|o| self.opt(o, state)).collect()
    }
}

impl Kind for MultiSelect {
    fn on_cursor(&mut self, action: &str) {
        let last = self.options.len().saturating_sub(1);
        match action {
            "left" | "up" => self.cursor = if self.cursor == 0 { last } else { self.cursor - 1 },
            "down" | "right" => self.cursor = if self.cursor == last { 0 } else { self.cursor + 1 },
            "space" => {
                let v = self.options[self.cursor].value.clone();
                if self.value.contains(&v) {
                    self.value.retain(|x| *x != v);
                } else {
                    self.value.push(v);
                }
            }
            _ => {}
        }
    }
    fn on_key(&mut self, ch: Option<&str>, _key: &Key) {
        if ch == Some("a") {
            let all = self.value.len() == self.options.len();
            self.value = if all { vec![] } else { self.options.iter().map(|o| o.value.clone()).collect() };
        }
    }
    fn validate(&self) -> Option<String> {
        let pc = self.pc;
        let key = |k: &str| pc.gray(&pc.bg_white(&pc.inverse(k)));
        (self.required && self.value.is_empty())
            .then(|| format!("Please select at least one option.\n{}", pc.reset(&pc.dim(&format!("Press {} to select, {} to submit", key(" space "), key(" enter "))))))
    }
    fn render(&self, p: &Ctx) -> String {
        let pc = p.pc;
        let title = format!("{}\n{}  {}\n", pc.gray(S_BAR), symbol(pc, p.state), self.message);
        let style = |o: &Opt, active: bool| {
            let selected = self.value.contains(&o.value);
            self.opt(
                o,
                match (active, selected) {
                    (true, true) => "active-selected",
                    (_, true) => "selected",
                    (true, false) => "active",
                    _ => "inactive",
                },
            )
        };
        match p.state {
            State::Submit => {
                let chosen = self.chosen("submitted").join(&pc.dim(", "));
                format!("{title}{}  {}", pc.gray(S_BAR), if chosen.is_empty() { pc.dim("none") } else { chosen })
            }
            State::Cancel => {
                let label = self.chosen("cancelled").join(&pc.dim(", "));
                let tail = if label.trim().is_empty() { String::new() } else { format!("{label}\n{}", pc.gray(S_BAR)) };
                format!("{title}{}  {tail}", pc.gray(S_BAR))
            }
            State::Error => {
                let footer = p
                    .error
                    .split('\n')
                    .enumerate()
                    .map(|(i, ln)| if i == 0 { format!("{}  {}", pc.yellow(S_BAR_END), pc.yellow(ln)) } else { format!("   {ln}") })
                    .collect::<Vec<_>>()
                    .join("\n");
                let bar = pc.yellow(S_BAR);
                format!("{title}{bar}  {}\n{footer}\n", limit_options(p, &self.options, self.cursor, self.max_items, &style).join(&format!("\n{bar}  ")))
            }
            _ => {
                let bar = pc.cyan(S_BAR);
                format!("{title}{bar}  {}\n{}\n", limit_options(p, &self.options, self.cursor, self.max_items, &style).join(&format!("\n{bar}  ")), pc.cyan(S_BAR_END))
            }
        }
    }
}

// ---------------------------------------------------------------------------------------------
// confirm (`core/src/prompts/confirm.ts`, `prompts/src/confirm.ts`)

pub struct Confirm {
    pub message: String,
    pub value: bool,
}

impl Confirm {
    pub fn new(message: &str, initial: bool) -> Confirm {
        Confirm { message: message.into(), value: initial }
    }
}

impl Kind for Confirm {
    fn on_cursor(&mut self, _action: &str) {
        self.value = !self.value;
    }
    fn on_confirm(&mut self, yes: bool) -> bool {
        self.value = yes;
        true
    }
    fn render(&self, p: &Ctx) -> String {
        let pc = p.pc;
        let title = format!("{}\n{}  {}\n", pc.gray(S_BAR), symbol(pc, p.state), self.message);
        let value = if self.value { "Yes" } else { "No" };
        match p.state {
            State::Submit => format!("{title}{}  {}", pc.gray(S_BAR), pc.dim(value)),
            State::Cancel => format!("{title}{}  {}\n{}", pc.gray(S_BAR), pc.strikethrough(&pc.dim(value)), pc.gray(S_BAR)),
            _ => {
                let option = |on: bool, label: &str| if on { format!("{} {label}", pc.green(S_RADIO_ACTIVE)) } else { format!("{} {}", pc.dim(S_RADIO_INACTIVE), pc.dim(label)) };
                format!("{title}{}  {} {} {}\n{}\n", pc.cyan(S_BAR), option(self.value, "Yes"), pc.dim("/"), option(!self.value, "No"), pc.cyan(S_BAR_END))
            }
        }
    }
}

// ---------------------------------------------------------------------------------------------
// text / password (`core/src/prompts/text.ts`, `password.ts`, `prompts/src/text.ts`, `password.ts`)

pub type Validate<'a> = &'a dyn Fn(Option<&str>) -> Option<String>;

pub struct Text<'a> {
    pub message: String,
    pub placeholder: Option<String>,
    pub default_value: Option<String>,
    pub value: Option<String>,
    pub validate: Option<Validate<'a>>,
}

impl<'a> Text<'a> {
    pub fn new(message: &str, validate: Option<Validate<'a>>) -> Text<'a> {
        Text { message: message.into(), placeholder: None, default_value: None, value: None, validate }
    }
}

fn input_with_cursor(pc: Pc, input: &str, cursor: usize) -> String {
    let chars: Vec<char> = input.chars().collect();
    if cursor >= chars.len() {
        return format!("{input}█");
    }
    let before: String = chars[..cursor].iter().collect();
    let after: String = chars[cursor + 1..].iter().collect();
    format!("{before}{}{after}", pc.inverse(&chars[cursor].to_string()))
}

impl Kind for Text<'_> {
    fn track(&self) -> bool {
        true
    }
    fn on_user_input(&mut self, input: &str) {
        self.value = Some(input.to_string());
    }
    fn validate(&self) -> Option<String> {
        self.validate.and_then(|v| v(self.value.as_deref()))
    }
    fn finalize(&mut self) {
        if self.value.as_deref().unwrap_or("").is_empty() {
            self.value = self.default_value.clone();
        }
        if self.value.is_none() {
            self.value = Some(String::new());
        }
    }
    fn render(&self, p: &Ctx) -> String {
        let pc = p.pc;
        let title = format!("{}\n{}  {}\n", pc.gray(S_BAR), symbol(pc, p.state), self.message);
        let placeholder = match self.placeholder.as_deref().filter(|s| !s.is_empty()) {
            Some(ph) => {
                let mut chars = ph.chars();
                let first = chars.next().map(String::from).unwrap_or_default();
                format!("{}{}", pc.inverse(&first), pc.dim(chars.as_str()))
            }
            None => pc.inverse(&pc.hidden("_")),
        };
        let user_input = if p.user_input.is_empty() {
            placeholder
        } else if p.state == State::Submit {
            p.user_input.clone()
        } else {
            input_with_cursor(pc, &p.user_input, p.cursor)
        };
        let value = self.value.clone().unwrap_or_default();
        match p.state {
            State::Error => format!("{}\n{}  {user_input}\n{}  {}\n", title.trim(), pc.yellow(S_BAR), pc.yellow(S_BAR_END), pc.yellow(&p.error)),
            State::Submit => format!("{title}{}  {}", pc.gray(S_BAR), pc.dim(&value)),
            State::Cancel => {
                let tail = if value.trim().is_empty() { String::new() } else { format!("\n{}", pc.gray(S_BAR)) };
                format!("{title}{}  {}{tail}", pc.gray(S_BAR), pc.strikethrough(&pc.dim(&value)))
            }
            _ => format!("{title}{}  {user_input}\n{}\n", pc.cyan(S_BAR), pc.cyan(S_BAR_END)),
        }
    }
}

pub struct Password<'a> {
    pub message: String,
    pub value: Option<String>,
    pub validate: Option<Validate<'a>>,
}

impl<'a> Password<'a> {
    pub fn new(message: &str, validate: Option<Validate<'a>>) -> Password<'a> {
        Password { message: message.into(), value: None, validate }
    }
}

impl Kind for Password<'_> {
    fn track(&self) -> bool {
        true
    }
    fn on_user_input(&mut self, input: &str) {
        self.value = Some(input.to_string());
    }
    fn validate(&self) -> Option<String> {
        self.validate.and_then(|v| v(self.value.as_deref()))
    }
    fn render(&self, p: &Ctx) -> String {
        let pc = p.pc;
        let title = format!("{}\n{}  {}\n", pc.gray(S_BAR), symbol(pc, p.state), self.message);
        let n = p.user_input.chars().count();
        let masked = S_PASSWORD_MASK.repeat(n);
        let user_input = if matches!(p.state, State::Submit | State::Cancel) {
            masked.clone()
        } else if p.cursor >= n {
            format!("{masked}{}", pc.inverse(&pc.hidden("_")))
        } else {
            format!("{}{}{}", S_PASSWORD_MASK.repeat(p.cursor), pc.inverse(S_PASSWORD_MASK), S_PASSWORD_MASK.repeat(n - p.cursor - 1))
        };
        match p.state {
            State::Error => format!("{}\n{}  {masked}\n{}  {}\n", title.trim(), pc.yellow(S_BAR), pc.yellow(S_BAR_END), pc.yellow(&p.error)),
            State::Submit => format!("{title}{}  {}", pc.gray(S_BAR), pc.dim(&masked)),
            State::Cancel => {
                let tail = if masked.is_empty() { String::new() } else { format!("\n{}", pc.gray(S_BAR)) };
                format!("{title}{}  {}{tail}", pc.gray(S_BAR), pc.strikethrough(&pc.dim(&masked)))
            }
            _ => format!("{title}{}  {user_input}\n{}\n", pc.cyan(S_BAR), pc.cyan(S_BAR_END)),
        }
    }
}

// ---------------------------------------------------------------------------------------------
// autocomplete (`core/src/prompts/autocomplete.ts`)

/// `AutocompletePrompt` state (single selection) over options identified by their values.
pub struct AutoState {
    values: Vec<String>,
    /// Indices of the options that match the search.
    pub filtered: Vec<usize>,
    /// The option cursor within `filtered`.
    pub cursor: usize,
    pub focused: Option<String>,
    /// `selectedValues`; an entry is `None` where upstream stores `undefined`.
    pub selected: Vec<Option<String>>,
    pub navigating: bool,
    last_user_input: String,
    /// `this.value`, set when Enter is pressed.
    pub value: Option<String>,
}

impl AutoState {
    pub fn new(values: Vec<String>, initial: Option<&str>) -> AutoState {
        let mut st = AutoState {
            filtered: (0..values.len()).collect(),
            values,
            cursor: 0,
            focused: None,
            selected: vec![],
            navigating: false,
            last_user_input: String::new(),
            value: None,
        };
        let initial: Vec<String> = match initial {
            Some(v) => vec![v.to_string()],
            None => st.values.first().cloned().into_iter().collect(),
        };
        for v in initial {
            if let Some(i) = st.values.iter().position(|x| *x == v) {
                st.toggle_selected(Some(v));
                st.cursor = i;
            }
        }
        st.focused = st.values.get(st.cursor).cloned();
        st
    }

    fn toggle_selected(&mut self, value: Option<String>) {
        if !self.filtered.is_empty() {
            self.selected = vec![value];
        }
    }

    fn focused_at(&self, cursor: usize) -> Option<String> {
        self.filtered.get(cursor).map(|&i| self.values[i].clone())
    }

    /// `#onUserInputChanged`.
    fn user_input(&mut self, input: &str, matches: &dyn Fn(&str, usize) -> bool) {
        if input == self.last_user_input {
            return;
        }
        self.last_user_input = input.to_string();
        self.filtered = (0..self.values.len()).filter(|&i| input.is_empty() || matches(input, i)).collect();
        self.cursor = match &self.focused {
            None => 0,
            Some(f) => self.filtered.iter().position(|&i| self.values[i] == *f).unwrap_or(0),
        };
        self.focused = self.focused_at(self.cursor);
        match self.focused.clone() {
            Some(f) => self.toggle_selected(Some(f)),
            None => self.selected.clear(),
        }
    }

    /// `#onKey`.
    fn key(&mut self, key: &Key) {
        match key.name.as_deref() {
            Some(n @ ("up" | "down")) => {
                let next = self.cursor as i64 + if n == "up" { -1 } else { 1 };
                self.cursor = next.min(self.filtered.len() as i64 - 1).max(0) as usize;
                self.focused = self.focused_at(self.cursor);
                self.selected = vec![self.focused.clone()];
                self.navigating = true;
            }
            Some("return") => self.value = self.selected.first().cloned().flatten(),
            _ => {
                if let Some(f) = self.focused.clone().filter(|f| !f.is_empty()) {
                    self.selected = vec![Some(f)];
                }
            }
        }
    }

    fn is_selected(&self, value: &str) -> bool {
        self.selected.iter().any(|s| s.as_deref() == Some(value))
    }
}

/// `userInputWithCursor` of `AutocompletePrompt`.
fn search_with_cursor(p: &Ctx) -> String {
    if p.user_input.is_empty() {
        return p.pc.inverse(&p.pc.hidden("_"));
    }
    input_with_cursor(p.pc, &p.user_input, p.cursor)
}

/// `autocomplete` from `prompts/src/autocomplete.ts`.
pub struct Autocomplete {
    pub message: String,
    pub options: Vec<Opt>,
    pub max_items: Option<usize>,
    pub placeholder: Option<String>,
    pub state: AutoState,
}

impl Autocomplete {
    pub fn new(message: &str, options: Vec<Opt>, max_items: Option<usize>) -> Autocomplete {
        let state = AutoState::new(options.iter().map(|o| o.value.clone()).collect(), None);
        Autocomplete { message: message.into(), options, max_items, placeholder: None, state }
    }
}

impl Kind for Autocomplete {
    fn track(&self) -> bool {
        true
    }
    fn on_user_input(&mut self, input: &str) {
        let options = &self.options;
        // `getFilteredOption`: label, hint or value contains the search, ignoring case.
        let matches = |search: &str, i: usize| {
            let o = &options[i];
            let term = search.to_lowercase();
            o.label.to_lowercase().contains(&term) || o.hint.as_deref().unwrap_or("").to_lowercase().contains(&term) || o.value.to_lowercase().contains(&term)
        };
        self.state.user_input(input, &matches);
    }
    fn on_key(&mut self, _ch: Option<&str>, key: &Key) {
        self.state.key(key);
    }
    fn render(&self, p: &Ctx) -> String {
        let pc = p.pc;
        let st = &self.state;
        let title = format!("{}\n{}  {}\n", pc.gray(S_BAR), symbol(pc, p.state), self.message);
        match p.state {
            State::Submit => {
                let label = self.options.iter().filter(|o| st.is_selected(&o.value)).map(|o| o.label.as_str()).collect::<Vec<_>>().join(", ");
                format!("{title}{}  {}", pc.gray(S_BAR), pc.dim(&label))
            }
            State::Cancel => format!("{title}{}  {}", pc.gray(S_BAR), pc.strikethrough(&pc.dim(&p.user_input))),
            _ => {
                let show_placeholder = st.value.as_deref().unwrap_or("").is_empty() && self.placeholder.is_some();
                let search = if st.navigating || show_placeholder {
                    pc.dim(if show_placeholder { self.placeholder.as_deref().unwrap_or("") } else { &p.user_input })
                } else {
                    search_with_cursor(p)
                };
                let n = st.filtered.len();
                let matches = if n != self.options.len() { pc.dim(&format!(" ({n} match{})", if n == 1 { "" } else { "es" })) } else { String::new() };
                let filtered: Vec<Opt> = st.filtered.iter().map(|&i| self.options[i].clone()).collect();
                let style = |o: &Opt, active: bool| {
                    let hint = match &o.hint {
                        Some(h) if !h.is_empty() && st.focused.as_deref() == Some(o.value.as_str()) => pc.dim(&format!(" ({h})")),
                        _ => String::new(),
                    };
                    if active { format!("{} {}{hint}", pc.green(S_RADIO_ACTIVE), o.label) } else { format!("{} {}{hint}", pc.dim(S_RADIO_INACTIVE), pc.dim(&o.label)) }
                };
                let options = if filtered.is_empty() { vec![] } else { limit_options(p, &filtered, st.cursor, self.max_items, &style) };
                let instructions = [format!("{} to select", pc.dim("↑/↓")), format!("{} confirm", pc.dim("Enter:")), format!("{} to search", pc.dim("Type:"))];
                let mut lines = vec![title, format!("{}  {} {search}{matches}", pc.cyan(S_BAR), pc.dim("Search:"))];
                if filtered.is_empty() && !p.user_input.is_empty() {
                    lines.push(format!("{}  {}", pc.cyan(S_BAR), pc.yellow("No matches found")));
                }
                if p.state == State::Error {
                    lines.push(format!("{}  {}", pc.yellow(S_BAR), pc.yellow(&p.error)));
                }
                lines.extend(options.iter().map(|o| format!("{}  {o}", pc.cyan(S_BAR))));
                lines.push(format!("{}  {}", pc.cyan(S_BAR), pc.dim(&instructions.join(" • "))));
                lines.push(pc.cyan(S_BAR_END));
                lines.join("\n")
            }
        }
    }
}

// ---------------------------------------------------------------------------------------------
// integration picker (upstream `cli/src/ui/integration-picker.ts`, on `AutocompletePrompt`)

#[derive(Clone, Debug)]
pub struct IntegrationChoice {
    pub value: String,
    pub label: String,
    /// "MCP", "Popular" or "Services".
    pub category: String,
    pub connected: bool,
    pub hint: Option<String>,
}

/// `selectIntegration(choices, kind)`.
pub struct IntegrationPicker {
    pub kind: String,
    pub choices: Vec<IntegrationChoice>,
    pub state: AutoState,
}

impl IntegrationPicker {
    pub fn new(choices: Vec<IntegrationChoice>, kind: &str) -> IntegrationPicker {
        let state = AutoState::new(choices.iter().map(|c| c.value.clone()).collect(), None);
        IntegrationPicker { kind: kind.into(), choices, state }
    }
}

impl Kind for IntegrationPicker {
    fn track(&self) -> bool {
        true
    }
    fn on_user_input(&mut self, input: &str) {
        let choices = &self.choices;
        let matches = |search: &str, i: usize| {
            let c = &choices[i];
            let term = search.to_lowercase();
            [&c.label, &c.value, &c.category].iter().any(|v| v.to_lowercase().contains(&term))
        };
        self.state.user_input(input, &matches);
    }
    fn on_key(&mut self, _ch: Option<&str>, key: &Key) {
        self.state.key(key);
    }
    fn validate(&self) -> Option<String> {
        self.state.value.as_deref().filter(|v| !v.is_empty()).is_none().then(|| format!("Select an {}", self.kind))
    }
    fn render(&self, p: &Ctx) -> String {
        let pc = p.pc;
        let st = &self.state;
        let title = format!("{}\n{}  Select {}", pc.gray(S_BAR), symbol(pc, p.state), self.kind);
        match p.state {
            State::Submit => {
                let label = self.choices.iter().find(|c| Some(c.value.as_str()) == st.value.as_deref()).map(|c| c.label.as_str()).unwrap_or("");
                return format!("{title}\n{}  {}", pc.gray(S_BAR), pc.dim(label));
            }
            State::Cancel => return format!("{title}\n{}  {}", pc.gray(S_BAR), pc.strikethrough(&pc.dim(&p.user_input))),
            _ => {}
        }
        let rows = p.rows.unwrap_or(24) as i64;
        let error = (p.state == State::Error) as i64;
        let max_items = 8.min(2.max(rows - 14 - error));
        let compact = rows < 18;
        let n = st.filtered.len() as i64;
        let start = (st.cursor as i64 - 2.min(max_items - 1)).max(0).min((n - max_items).max(0));
        let visible: Vec<&IntegrationChoice> = st.filtered.iter().skip(start as usize).take(max_items as usize).map(|&i| &self.choices[i]).collect();
        let bar = pc.cyan(S_BAR);
        let mut items: Vec<String> = vec![];
        for (index, c) in visible.iter().enumerate() {
            if index == 0 || visible[index - 1].category != c.category {
                if !compact {
                    items.push(format!("{bar}  "));
                }
                items.push(format!("{bar}  {}", pc.bold(&c.category)));
            }
            let active = start + index as i64 == st.cursor as i64;
            items.push(format!(
                "{bar}  {} {}{}{}",
                if active { pc.green(S_RADIO_ACTIVE) } else { pc.dim(S_RADIO_INACTIVE) },
                if active { c.label.clone() } else { pc.dim(&c.label) },
                if c.connected { format!(" {}", pc.green("✓")) } else { String::new() },
                c.hint.as_deref().filter(|h| !h.is_empty()).map(|h| format!(" {}", pc.dim(&format!("({h})")))).unwrap_or_default(),
            ));
        }
        let mut lines = vec![title, format!("{bar}  {} {}", pc.dim("Search:"), if st.navigating { pc.dim(&p.user_input) } else { search_with_cursor(p) })];
        if visible.is_empty() && !p.user_input.is_empty() {
            lines.push(format!("{bar}  {}", pc.yellow(&format!("No {}s found", self.kind))));
        }
        if p.state == State::Error && !visible.is_empty() {
            lines.push(format!("{}  {}", pc.yellow(S_BAR), pc.yellow(&p.error)));
        }
        if start > 0 {
            lines.push(format!("{bar}  {}", pc.dim("…")));
        }
        lines.extend(items);
        if start + max_items < n {
            lines.push(format!("{bar}  {}", pc.dim("…")));
        }
        let help = if p.columns.unwrap_or(80) < 50 { "↑/↓ navigate • Enter select" } else { "↑/↓ to select • Enter: confirm • Type: to search" };
        lines.push(format!("{bar}  {}", pc.dim(help)));
        lines.push(pc.cyan(S_BAR_END));
        lines.join("\n")
    }
}

#[cfg(test)]
mod tests;
