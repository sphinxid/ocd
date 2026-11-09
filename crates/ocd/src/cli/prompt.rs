//! `effect/cli` 4.0.6 prompts (`Prompt.ts`, `internal/ansi.ts`) as the command wizard runs them through
//! `@effect/platform-node-shared` 4.0.6 `NodeTerminal`: `Confirm`, `Toggle`, `Select`, `String`,
//! `Password`, `Hidden`, `Int`, `Number` and `Date`, each a render/process/clear state machine driven by
//! [`run`] exactly like `runLoop` (render, read a key, beep / next frame / submit, clear the previous
//! frame by its plain text measured at the current width). Not clack: effect's prompts redraw the whole
//! frame on every key, never re-render on resize, and quit on escape, ctrl+c and ctrl+d.

use std::collections::VecDeque;

/// `internal/ansi.ts`.
pub mod ansi {
    pub const RESET: &str = "\x1b[0m";
    pub const BOLD: &str = "\x1b[1m";
    pub const ITALICIZED: &str = "\x1b[3m";
    pub const UNDERLINED: &str = "\x1b[4m";
    pub const STRIKETHROUGH: &str = "\x1b[9m";
    pub const CURSOR_SHOW: &str = "\x1b[?25h";
    pub const CURSOR_HIDE: &str = "\x1b[?25l";
    pub const CURSOR_LEFT: &str = "\x1b[G";
    pub const CURSOR_SAVE_POSITION: &str = "\x1b[s";
    pub const CURSOR_RESTORE_POSITION: &str = "\x1b[u";
    pub const ERASE_LINE: &str = "\x1b[2K";
    pub const BEEP: &str = "\x07";
    pub const RED: &str = "\x1b[31m";
    pub const GREEN: &str = "\x1b[32m";
    pub const MAGENTA: &str = "\x1b[35m";
    pub const WHITE: &str = "\x1b[37m";
    pub const BLACK_BRIGHT: &str = "\x1b[90m";
    pub const CYAN_BRIGHT: &str = "\x1b[96m";

    /// `annotate`: the styles, the text and a reset; the bare text when colors are off.
    pub fn annotate(colors: bool, text: &str, styles: &[&str]) -> String {
        if !colors {
            return text.to_string();
        }
        format!("{}{text}{RESET}", styles.concat())
    }

    /// `cursorTo(column)` (no row).
    pub fn cursor_to(column: i64) -> String {
        format!("\x1b[{}G", (column + 1).max(0))
    }

    pub fn cursor_down(lines: usize) -> String {
        format!("\x1b[{lines}B")
    }

    /// `cursorMove(column)` (row 0).
    pub fn cursor_move(column: i64) -> String {
        if column > 0 {
            format!("\x1b[{column}C")
        } else if column < 0 {
            format!("\x1b[{}D", -column)
        } else {
            String::new()
        }
    }

    pub fn erase_lines(rows: usize) -> String {
        let mut command = String::new();
        for i in 0..rows {
            command.push_str("\x1b[2K");
            if i + 1 < rows {
                command.push_str("\x1b[1A");
            }
        }
        if rows > 0 {
            command.push_str("\x1b[G");
        }
        command
    }

    /// `detectColors`, the default `Prompt.Theme` colors (read once): stdout is a TTY and `NO_COLOR` is unset
    /// or empty. Unlike help (`TerminalCapabilities`), `FORCE_COLOR` and `TERM=dumb` play no part.
    pub fn detect_colors() -> bool {
        crate::term::stdout_is_tty() && std::env::var("NO_COLOR").map(|v| v.is_empty()).unwrap_or(true)
    }
}

/// JS `string.length` (UTF-16 code units).
pub fn js_len(s: &str) -> usize {
    s.encode_utf16().count()
}

/// `Prompt.Theme` (the platform defaults of `makeTheme`).
#[derive(Clone, Debug)]
pub struct Theme {
    pub prefix: &'static str,
    pub pointer: &'static str,
    pub pointer_small: &'static str,
    pub ellipsis: &'static str,
    pub tick: &'static str,
    pub arrow_up: &'static str,
    pub arrow_down: &'static str,
    pub description_separator: &'static str,
    pub password_mask: &'static str,
    pub toggle_separator: &'static str,
    pub primary_color: &'static str,
    pub muted_color: &'static str,
    pub success_color: &'static str,
    pub error_color: &'static str,
    pub submitted_color: &'static str,
    pub colors: bool,
}

impl Theme {
    /// `defaultTheme`, or `windowsTheme` on Windows.
    pub fn new(colors: bool) -> Theme {
        let windows = cfg!(windows);
        Theme {
            prefix: "?",
            pointer: if windows { ">" } else { "❯" },
            pointer_small: if windows { "»" } else { "›" },
            ellipsis: if windows { "..." } else { "…" },
            tick: if windows { "√" } else { "✔" },
            arrow_up: "↑",
            arrow_down: "↓",
            description_separator: "- ",
            password_mask: "*",
            toggle_separator: "/",
            primary_color: ansi::CYAN_BRIGHT,
            muted_color: ansi::BLACK_BRIGHT,
            success_color: ansi::GREEN,
            error_color: ansi::RED,
            submitted_color: ansi::WHITE,
            colors,
        }
    }

    fn annotate(&self, text: &str, styles: &[&str]) -> String {
        ansi::annotate(self.colors, text, styles)
    }

    fn annotate_error_line(&self, line: &str) -> String {
        self.annotate(line, &[ansi::ITALICIZED, self.error_color])
    }

    fn annotate_symbol(&self, symbol: &str, styles: &[&str]) -> String {
        if symbol.is_empty() { String::new() } else { self.annotate(symbol, styles) }
    }

    fn paging_prefix(&self, show_up: bool, show_down: bool) -> String {
        let width = js_len(self.arrow_up).max(js_len(self.arrow_down));
        let pad = |s: &str| format!("{s}{}", " ".repeat(width.saturating_sub(js_len(s))));
        if show_up {
            pad(self.arrow_up)
        } else if show_down {
            pad(self.arrow_down)
        } else {
            " ".repeat(width)
        }
    }
}

fn separate_symbol(symbol: &str, text: &str) -> String {
    if symbol.is_empty() { text.to_string() } else { format!("{symbol} {text}") }
}

/// `text.split(/\r?\n/)`.
fn split_lines(text: &str) -> Vec<&str> {
    text.split('\n').map(|l| l.strip_suffix('\r').unwrap_or(l)).collect()
}

/// `renderPrompt`: `<leading> <message> <trailing> <confirm>`, the message bold unless plain.
fn render_prompt(confirm: &str, message: &str, leading: &str, trailing: &str, theme: &Theme, plain: bool) -> String {
    let prefix = if leading.is_empty() { String::new() } else { format!("{leading} ") };
    let lines: Vec<String> =
        split_lines(message).into_iter().map(|line| if plain { line.to_string() } else { theme.annotate(line, &[ansi::BOLD]) }).collect();
    let mut output = prefix + &lines.join("\n");
    if !trailing.is_empty() {
        output.push(' ');
        output.push_str(trailing);
    }
    if !confirm.is_empty() {
        output.push(' ');
        output.push_str(confirm);
    } else if !trailing.is_empty() {
        output.push(' ');
    }
    output
}

/// `eraseText`: clears every row the plain `text` takes at `columns`.
fn erase_text(text: &str, columns: usize) -> String {
    if columns == 0 {
        return format!("{}{}", ansi::ERASE_LINE, ansi::cursor_to(0));
    }
    let rows: usize = split_lines(text).iter().map(|line| 1 + js_len(line).saturating_sub(1) / columns).sum();
    ansi::erase_lines(rows)
}

/// `lines`: rows the text takes (`ceil(length / columns)` per line).
fn count_lines(text: &str, columns: usize) -> usize {
    let lines = split_lines(text);
    if columns == 0 {
        return lines.len();
    }
    lines.iter().map(|line| js_len(line).div_ceil(columns)).sum()
}

fn clear_output_with_error(output: &str, columns: usize, error: &str) -> String {
    if !error.is_empty() {
        return ansi::cursor_down(count_lines(error, columns)) + &erase_text(&format!("\n{error}"), columns) + &erase_text(output, columns);
    }
    erase_text(output, columns)
}

/// The error lines under a prompt (saved cursor, newline, pointer + italic error, restored cursor), or the
/// plain text the clear step measures.
fn render_error(error: Option<&str>, theme: &Theme, plain: bool) -> String {
    let Some(error) = error else { return String::new() };
    let lines = split_lines(error);
    if plain {
        return separate_symbol(theme.pointer_small, &lines.join("\n"));
    }
    let prefix = theme.annotate_symbol(theme.pointer_small, &[theme.error_color]);
    let lines: Vec<String> = lines.iter().map(|l| theme.annotate_error_line(l)).collect();
    format!("{}\n{}{}", ansi::CURSOR_SAVE_POSITION, separate_symbol(&prefix, &lines.join("\n")), ansi::CURSOR_RESTORE_POSITION)
}

fn reset_line() -> String {
    format!("{}{}", ansi::ERASE_LINE, ansi::CURSOR_LEFT)
}

// ---------------------------------------------------------------------------------------------
// input and the run loop

/// `Terminal.UserInput`: the keypress string (absent for escape sequences) and the key.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct UserInput {
    pub input: Option<String>,
    pub name: String,
    pub ctrl: bool,
    pub meta: bool,
    pub shift: bool,
}

impl UserInput {
    pub fn from_key(k: &crate::clack::Key) -> UserInput {
        UserInput { input: k.ch.clone(), name: k.name.clone().unwrap_or_default(), ctrl: k.ctrl, meta: k.meta, shift: k.shift }
    }

    fn text(&self) -> &str {
        self.input.as_deref().unwrap_or("")
    }
}

/// `NodeTerminal`'s `defaultShouldQuit`: escape, ctrl+c and ctrl+d end the input.
pub fn should_quit(input: &UserInput) -> bool {
    input.name == "escape" || (input.ctrl && (input.name == "c" || input.name == "d"))
}

/// The `Terminal` service as the prompts and the wizard use it.
pub trait Terminal {
    /// `stdout.columns ?? 0`.
    fn columns(&mut self) -> usize;
    /// A new `readInput` queue for one prompt: keys read for an earlier prompt are gone.
    fn begin(&mut self);
    /// The next keypress; `None` once the queue is done (a quit key was read, or the input ended).
    fn read_input(&mut self) -> Option<UserInput>;
    /// `terminal.display` (stdout).
    fn display(&mut self, s: &str);
    /// `Console.log(s)` (stdout).
    fn log(&mut self, s: &str) {
        self.display(&format!("{s}\n"));
    }
}

/// `Terminal.QuitError`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Quit;

pub enum Action<S, O> {
    Beep,
    NextFrame(S),
    Submit(O),
}

/// `Prompt.Handlers` of a `Custom` prompt.
pub trait Handlers {
    type State: Clone;
    type Output: Clone;
    fn initial(&self) -> Self::State;
    fn render(&self, state: &Self::State, action: &Action<Self::State, Self::Output>) -> String;
    fn process(&self, input: &UserInput, state: &Self::State) -> Action<Self::State, Self::Output>;
    /// Clears the frame of `state` before the next one; `action` is what replaces it (a `Date` prompt's
    /// parts are edited in place upstream, so the old frame is measured with the new value).
    fn clear(&self, state: &Self::State, action: &Action<Self::State, Self::Output>, columns: usize) -> String;
}

/// `Prompt.run` (`runLoop`): renders, reads keys and processes them until the prompt submits; the cursor
/// is shown again however it ends.
pub fn run<H: Handlers>(prompt: &H, term: &mut dyn Terminal) -> Result<H::Output, Quit> {
    term.begin();
    let mut state = prompt.initial();
    let mut action = Action::NextFrame(state.clone());
    let mut clear = String::new();
    let result = loop {
        let msg = prompt.render(&state, &action);
        term.display(&format!("{clear}{msg}"));
        clear.clear();
        let Some(input) = term.read_input() else { break Err(Quit) };
        action = prompt.process(&input, &state);
        match &action {
            Action::Beep => continue,
            Action::NextFrame(next) => {
                let columns = term.columns();
                clear = prompt.clear(&state, &action, columns);
                state = next.clone();
            }
            Action::Submit(value) => {
                let columns = term.columns();
                let clear = prompt.clear(&state, &action, columns);
                let msg = prompt.render(&state, &action);
                term.display(&format!("{clear}{msg}"));
                break Ok(value.clone());
            }
        }
    };
    term.display(ansi::CURSOR_SHOW);
    result
}

// ---------------------------------------------------------------------------------------------
// Confirm

pub struct Confirm {
    pub message: String,
    pub initial: bool,
    pub label_confirm: String,
    pub label_deny: String,
    pub placeholder_confirm: String,
    pub placeholder_deny: String,
    pub theme: Theme,
}

impl Confirm {
    /// `Prompt.Confirm({ message, initial })` with the default labels and placeholders.
    pub fn new(message: &str, initial: bool, theme: &Theme) -> Confirm {
        Confirm {
            message: message.into(),
            initial,
            label_confirm: "yes".into(),
            label_deny: "no".into(),
            placeholder_confirm: "(Y/n)".into(),
            placeholder_deny: "(y/N)".into(),
            theme: theme.clone(),
        }
    }

    fn placeholder(&self, value: bool) -> &str {
        if value { &self.placeholder_confirm } else { &self.placeholder_deny }
    }
}

impl Handlers for Confirm {
    type State = bool;
    type Output = bool;

    fn initial(&self) -> bool {
        self.initial
    }

    fn render(&self, _: &bool, action: &Action<bool, bool>) -> String {
        let t = &self.theme;
        match action {
            Action::Beep => ansi::BEEP.into(),
            Action::NextFrame(state) => {
                let leading = t.annotate_symbol(t.prefix, &[t.primary_color]);
                let trailing = t.annotate_symbol(t.pointer_small, &[t.muted_color]);
                let confirm = t.annotate(self.placeholder(*state), &[t.muted_color]);
                format!("{}{}", ansi::CURSOR_HIDE, render_prompt(&confirm, &self.message, &leading, &trailing, t, false))
            }
            Action::Submit(value) => {
                let leading = t.annotate_symbol(t.tick, &[t.success_color]);
                let trailing = t.annotate_symbol(t.ellipsis, &[t.muted_color]);
                let label = if *value { &self.label_confirm } else { &self.label_deny };
                render_prompt(label, &self.message, &leading, &trailing, t, false) + "\n"
            }
        }
    }

    fn process(&self, input: &UserInput, _: &bool) -> Action<bool, bool> {
        if input.name == "enter" || input.name == "return" {
            return Action::Submit(self.initial);
        }
        // `/^y|t$/` and `/^n|f$/`: starts with y / ends with t, starts with n / ends with f.
        let value = input.text().to_lowercase();
        if value.starts_with('y') || value.ends_with('t') {
            return Action::Submit(true);
        }
        if value.starts_with('n') || value.ends_with('f') {
            return Action::Submit(false);
        }
        Action::Beep
    }

    fn clear(&self, state: &bool, _: &Action<bool, bool>, columns: usize) -> String {
        let t = &self.theme;
        let text = render_prompt(self.placeholder(*state), &self.message, t.prefix, t.pointer_small, t, true);
        erase_text(&text, columns) + &reset_line()
    }
}

// ---------------------------------------------------------------------------------------------
// Toggle

pub struct Toggle {
    pub message: String,
    pub initial: bool,
    pub active: String,
    pub inactive: String,
    pub theme: Theme,
}

impl Toggle {
    fn render_toggle(&self, value: bool, submitted: bool) -> String {
        let t = &self.theme;
        let separator = t.annotate_symbol(t.toggle_separator, &[t.muted_color]);
        let selected = [ansi::UNDERLINED, if submitted { t.submitted_color } else { t.primary_color }];
        let inactive = if value { self.inactive.clone() } else { t.annotate(&self.inactive, &selected) };
        let active = if value { t.annotate(&self.active, &selected) } else { self.active.clone() };
        format!("{active} {}", separate_symbol(&separator, &inactive))
    }
}

impl Handlers for Toggle {
    type State = bool;
    type Output = bool;

    fn initial(&self) -> bool {
        self.initial
    }

    fn render(&self, state: &bool, action: &Action<bool, bool>) -> String {
        let t = &self.theme;
        match action {
            Action::Beep => ansi::BEEP.into(),
            Action::NextFrame(_) => {
                let leading = t.annotate_symbol(t.prefix, &[t.primary_color]);
                let trailing = t.annotate_symbol(t.pointer_small, &[t.muted_color]);
                let toggle = self.render_toggle(*state, false);
                format!("{}{}", ansi::CURSOR_HIDE, render_prompt(&toggle, &self.message, &leading, &trailing, t, false))
            }
            Action::Submit(_) => {
                let leading = t.annotate_symbol(t.tick, &[t.success_color]);
                let trailing = t.annotate_symbol(t.ellipsis, &[t.muted_color]);
                let toggle = self.render_toggle(*state, true);
                render_prompt(&toggle, &self.message, &leading, &trailing, t, false) + "\n"
            }
        }
    }

    fn process(&self, input: &UserInput, state: &bool) -> Action<bool, bool> {
        match input.name.as_str() {
            "0" | "j" | "delete" | "right" | "down" => Action::NextFrame(false),
            "1" | "k" | "left" | "up" => Action::NextFrame(true),
            // `" "` never matches: readline names the space key "space".
            " " | "tab" => Action::NextFrame(!state),
            "enter" | "return" => Action::Submit(*state),
            _ => Action::Beep,
        }
    }

    fn clear(&self, _: &bool, _: &Action<bool, bool>, columns: usize) -> String {
        let t = &self.theme;
        let toggle = format!("{} {}", self.active, separate_symbol(t.toggle_separator, &self.inactive));
        let text = render_prompt(&toggle, &self.message, t.prefix, t.pointer_small, t, true);
        erase_text(&text, columns) + &reset_line()
    }
}

// ---------------------------------------------------------------------------------------------
// Select

#[derive(Clone, Debug, Default)]
pub struct Choice {
    pub title: String,
    pub description: Option<String>,
    pub disabled: bool,
}

pub struct Select {
    pub message: Option<String>,
    pub choices: Vec<Choice>,
    pub max_per_page: usize,
    pub theme: Theme,
}

/// `entriesToDisplay`.
fn entries_to_display(cursor: usize, total: usize, max: usize) -> (usize, usize) {
    let start = (total as i64 - max as i64).min(cursor as i64 - (max / 2) as i64).max(0) as usize;
    (start, (start + max).min(total))
}

impl Select {
    pub fn new(message: Option<&str>, choices: Vec<Choice>, theme: &Theme) -> Select {
        Select { message: message.map(String::from), choices, max_per_page: 10, theme: theme.clone() }
    }

    fn output(&self, leading: &str, trailing: &str, plain: bool) -> Option<String> {
        self.message.as_ref().map(|m| render_prompt("", m, leading, trailing, &self.theme, plain))
    }

    fn render_choices(&self, state: usize, plain: bool) -> String {
        let t = &self.theme;
        let (start, end) = entries_to_display(state, self.choices.len(), self.max_per_page);
        let mut docs = vec![];
        for index in start..end {
            let choice = &self.choices[index];
            let selected = state == index;
            let paging = t.paging_prefix(index == start && start > 0, index + 1 == end && end < self.choices.len());
            let blank = " ".repeat(js_len(t.pointer));
            let prefix = if plain {
                if selected { format!("{}{paging}", t.pointer) } else { format!("{paging}{blank}") }
            } else if choice.disabled {
                if selected { t.annotate_symbol(t.pointer, &[ansi::BOLD, t.muted_color]) + &paging } else { format!("{paging}{blank}") }
            } else if selected {
                t.annotate_symbol(t.pointer, &[t.primary_color]) + &paging
            } else {
                format!("{paging}{blank}")
            };
            let title = if plain {
                choice.title.clone()
            } else if selected {
                t.annotate(&choice.title, &[ansi::UNDERLINED, if choice.disabled { t.muted_color } else { t.primary_color }])
            } else if choice.disabled {
                t.annotate(&choice.title, &[ansi::STRIKETHROUGH, t.muted_color])
            } else {
                choice.title.clone()
            };
            let description = match &choice.description {
                Some(d) if !choice.disabled && !d.is_empty() && selected => {
                    let text = format!("{}{d}", t.description_separator);
                    if plain { text } else { t.annotate(&text, &[t.muted_color]) }
                }
                _ => String::new(),
            };
            docs.push(format!("{prefix}{title} {description}"));
        }
        docs.join("\n")
    }
}

fn with_header(header: Option<String>, body: String) -> String {
    match header {
        Some(h) => format!("{h}\n{body}"),
        None => body,
    }
}

impl Handlers for Select {
    type State = usize;
    type Output = usize;

    fn initial(&self) -> usize {
        0
    }

    fn render(&self, state: &usize, action: &Action<usize, usize>) -> String {
        let t = &self.theme;
        match action {
            Action::Beep => ansi::BEEP.into(),
            Action::NextFrame(state) => {
                let choices = self.render_choices(*state, false);
                let leading = t.annotate_symbol(t.prefix, &[t.primary_color]);
                let trailing = t.annotate_symbol(t.pointer_small, &[t.muted_color]);
                format!("{}{}", ansi::CURSOR_HIDE, with_header(self.output(&leading, &trailing, false), choices))
            }
            Action::Submit(_) => {
                let selected = &self.choices[*state].title;
                let leading = t.annotate_symbol(t.tick, &[t.success_color]);
                let trailing = t.annotate_symbol(t.ellipsis, &[t.muted_color]);
                let msg = self.output(&leading, &trailing, false).unwrap_or(leading);
                format!("{msg} {}\n", t.annotate(selected, &[t.submitted_color]))
            }
        }
    }

    fn process(&self, input: &UserInput, state: &usize) -> Action<usize, usize> {
        let n = self.choices.len();
        match input.name.as_str() {
            "k" | "up" => Action::NextFrame(if *state == 0 { n - 1 } else { state - 1 }),
            "j" | "down" => Action::NextFrame(if *state + 1 == n { 0 } else { state + 1 }),
            "tab" => Action::NextFrame((state + 1) % n),
            "enter" | "return" => {
                if self.choices[*state].disabled {
                    Action::Beep
                } else {
                    Action::Submit(*state)
                }
            }
            _ => Action::Beep,
        }
    }

    fn clear(&self, state: &usize, _: &Action<usize, usize>, columns: usize) -> String {
        let t = &self.theme;
        let text = with_header(self.output(t.prefix, t.pointer_small, true), self.render_choices(*state, true));
        erase_text(&text, columns) + &reset_line()
    }
}

// ---------------------------------------------------------------------------------------------
// String / Password / Hidden

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum TextKind {
    Text,
    /// `Prompt.Password` (`Redacted` parameters, which ocd's commands do not have).
    #[cfg_attr(not(test), allow(dead_code))]
    Password,
    /// `Prompt.Hidden`.
    #[cfg_attr(not(test), allow(dead_code))]
    Hidden,
}

pub type TextValidate = Box<dyn Fn(&str) -> Result<String, String>>;

pub struct Text {
    pub message: String,
    pub default: String,
    pub kind: TextKind,
    pub validate: TextValidate,
    pub theme: Theme,
}

/// Text state: the value and cursor in UTF-16 code units, like the JS string it ports.
#[derive(Clone, Debug)]
pub struct TextState {
    cursor: usize,
    value: Vec<u16>,
    error: Option<String>,
}

impl Text {
    pub fn new(message: &str, kind: TextKind, theme: &Theme) -> Text {
        Text { message: message.into(), default: String::new(), kind, validate: Box::new(|v| Ok(v.to_string())), theme: theme.clone() }
    }

    fn input(&self, state: &TextState, submitted: bool, plain: bool) -> String {
        let t = &self.theme;
        let text = String::from_utf16_lossy(&state.value);
        if plain {
            return match self.kind {
                TextKind::Hidden => String::new(),
                TextKind::Password => t.password_mask.repeat(state.value.len()),
                TextKind::Text => text,
            };
        }
        if state.value.is_empty() {
            return String::new();
        }
        let annotation: Vec<&str> = if state.error.is_some() {
            vec![t.error_color]
        } else if submitted {
            vec![t.submitted_color]
        } else {
            vec![ansi::UNDERLINED, t.primary_color]
        };
        match self.kind {
            TextKind::Hidden => String::new(),
            TextKind::Password => t.annotate_symbol(&t.password_mask.repeat(state.value.len()), &annotation),
            TextKind::Text => t.annotate(&text, &annotation),
        }
    }

    fn output(&self, state: &TextState, leading: &str, trailing: &str, plain: bool, submitted: bool) -> String {
        render_prompt(&self.input(state, submitted, plain), &self.message, leading, trailing, &self.theme, plain)
    }

    fn next(state: &TextState, cursor: usize, value: Vec<u16>) -> Action<TextState, String> {
        let _ = state;
        Action::NextFrame(TextState { cursor, value, error: None })
    }
}

impl Handlers for Text {
    type State = TextState;
    type Output = String;

    fn initial(&self) -> TextState {
        let value: Vec<u16> = self.default.encode_utf16().collect();
        TextState { cursor: value.len(), value, error: None }
    }

    fn render(&self, state: &TextState, action: &Action<TextState, String>) -> String {
        let t = &self.theme;
        match action {
            Action::Beep => ansi::BEEP.into(),
            Action::NextFrame(state) => {
                let leading = t.annotate_symbol(t.prefix, &[t.primary_color]);
                let trailing = t.annotate_symbol(t.pointer_small, &[t.muted_color]);
                let prompt = self.output(state, &leading, &trailing, false, false);
                let error = render_error(state.error.as_deref(), t, false);
                let width = if self.kind == TextKind::Password { js_len(t.password_mask) } else { 1 } as i64;
                let offset = (state.cursor as i64 - state.value.len() as i64) * width;
                format!("{prompt}{error}{}", ansi::cursor_move(offset))
            }
            Action::Submit(_) => {
                let leading = t.annotate_symbol(t.tick, &[t.success_color]);
                let trailing = t.annotate_symbol(t.ellipsis, &[t.muted_color]);
                self.output(state, &leading, &trailing, false, true) + "\n"
            }
        }
    }

    fn process(&self, input: &UserInput, state: &TextState) -> Action<TextState, String> {
        if input.ctrl {
            return match input.name.as_str() {
                "u" => Text::next(state, 0, vec![]),
                "a" => Action::NextFrame(TextState { cursor: 0, error: None, ..state.clone() }),
                "e" => Action::NextFrame(TextState { cursor: state.value.len(), error: None, ..state.clone() }),
                _ => Action::Beep,
            };
        }
        match input.name.as_str() {
            "backspace" => {
                if state.cursor == 0 {
                    return Action::Beep;
                }
                let mut value = state.value[..state.cursor - 1].to_vec();
                value.extend_from_slice(&state.value[state.cursor..]);
                Text::next(state, state.cursor - 1, value)
            }
            "left" => {
                if state.cursor == 0 {
                    return Action::Beep;
                }
                Action::NextFrame(TextState { cursor: state.cursor - 1, error: None, ..state.clone() })
            }
            "right" => {
                if state.cursor >= state.value.len() {
                    return Action::Beep;
                }
                Action::NextFrame(TextState { cursor: state.cursor + 1, error: None, ..state.clone() })
            }
            "home" => Action::NextFrame(TextState { cursor: 0, error: None, ..state.clone() }),
            "end" => Action::NextFrame(TextState { cursor: state.value.len(), error: None, ..state.clone() }),
            "enter" | "return" => match (self.validate)(&String::from_utf16_lossy(&state.value)) {
                Ok(value) => Action::Submit(value),
                Err(error) => Action::NextFrame(TextState { error: Some(error), ..state.clone() }),
            },
            "tab" => {
                let default: Vec<u16> = self.default.encode_utf16().collect();
                if state.value == default {
                    return Action::Beep;
                }
                let value = if state.value.is_empty() { default } else { state.value.clone() };
                Text::next(state, value.len(), value)
            }
            _ => {
                let typed: Vec<u16> = input.text().encode_utf16().collect();
                let mut value = state.value[..state.cursor].to_vec();
                value.extend_from_slice(&typed);
                value.extend_from_slice(&state.value[state.cursor..]);
                Text::next(state, state.cursor + typed.len(), value)
            }
        }
    }

    fn clear(&self, state: &TextState, _: &Action<TextState, String>, columns: usize) -> String {
        let t = &self.theme;
        let error = render_error(state.error.as_deref(), t, true);
        clear_output_with_error(&self.output(state, t.prefix, t.pointer_small, true, false), columns, &error) + &reset_line()
    }
}

// ---------------------------------------------------------------------------------------------
// Int / Number

/// JS `String(number)`.
pub fn js_number(n: f64) -> String {
    if n.is_nan() {
        "NaN".into()
    } else if n.is_infinite() {
        if n > 0.0 { "Infinity".into() } else { "-Infinity".into() }
    } else {
        oc_server::support::js_number_string(n)
    }
}

fn js_trim_start(s: &str) -> &str {
    s.trim_start_matches(|c: char| c.is_whitespace() || c == '\u{feff}')
}

/// JS `Number.parseInt(s)` (radix detection included: a `0x` prefix reads hexadecimal).
pub fn js_parse_int(s: &str) -> f64 {
    let s = js_trim_start(s);
    let (negative, rest) = match s.as_bytes().first() {
        Some(b'-') => (true, &s[1..]),
        Some(b'+') => (false, &s[1..]),
        _ => (false, s),
    };
    let (radix, rest) = if rest.starts_with("0x") || rest.starts_with("0X") { (16, &rest[2..]) } else { (10, rest) };
    let digits: String = rest.chars().take_while(|c| c.is_digit(radix)).collect();
    if digits.is_empty() {
        return f64::NAN;
    }
    let value = if radix == 10 {
        digits.parse::<f64>().unwrap_or(f64::NAN)
    } else {
        digits.chars().fold(0.0, |acc, c| acc * 16.0 + c.to_digit(16).unwrap_or(0) as f64)
    };
    if negative { -value } else { value }
}

/// JS `Number.parseFloat(s)`: the longest `StrDecimalLiteral` prefix.
pub fn js_parse_float(s: &str) -> f64 {
    static RE: std::sync::LazyLock<regex::Regex> =
        std::sync::LazyLock::new(|| regex::Regex::new(r"^[+-]?(?:Infinity|(?:[0-9]+\.?[0-9]*|\.[0-9]+)(?:[eE][+-]?[0-9]+)?)").unwrap());
    let s = js_trim_start(s);
    let Some(m) = RE.find(s) else { return f64::NAN };
    let text = m.as_str();
    if let Some(sign) = text.strip_suffix("Infinity") {
        return if sign == "-" { f64::NEG_INFINITY } else { f64::INFINITY };
    }
    text.parse::<f64>().unwrap_or(f64::NAN)
}

/// JS `Math.round`.
fn js_round(x: f64) -> f64 {
    if !x.is_finite() || x == 0.0 {
        return x;
    }
    let r = (x + 0.5).floor();
    if r == 0.0 && x < 0.0 { -0.0 } else { r }
}

pub type NumberValidate = Box<dyn Fn(f64) -> Result<f64, String>>;

/// `Prompt.Int` (`precision: None`) and `Prompt.Number`.
pub struct Number {
    pub message: String,
    pub default: Option<f64>,
    pub min: f64,
    pub max: f64,
    pub increment_by: f64,
    pub decrement_by: f64,
    /// `Some` for `Prompt.Number` (defaults to 2).
    pub precision: Option<i32>,
    pub validate: Option<NumberValidate>,
    pub theme: Theme,
}

#[derive(Clone, Debug)]
pub struct NumberState {
    value: String,
    error: Option<String>,
}

impl Number {
    pub fn int(message: &str, theme: &Theme) -> Number {
        Number {
            message: message.into(),
            default: None,
            min: f64::NEG_INFINITY,
            max: f64::INFINITY,
            increment_by: 1.0,
            decrement_by: 1.0,
            precision: None,
            validate: None,
            theme: theme.clone(),
        }
    }

    /// `Prompt.Number` (`Finite` parameters, which ocd's commands do not have).
    #[cfg_attr(not(test), allow(dead_code))]
    pub fn float(message: &str, theme: &Theme) -> Number {
        Number { precision: Some(2), ..Number::int(message, theme) }
    }

    fn check(&self, n: f64) -> Result<f64, String> {
        if let Some(v) = &self.validate {
            return v(n);
        }
        if n < self.min {
            return Err(format!("{} must be greater than or equal to {}", js_number(n), js_number(self.min)));
        }
        if n > self.max {
            return Err(format!("{} must be less than or equal to {}", js_number(n), js_number(self.max)));
        }
        Ok(n)
    }

    fn parse(&self, s: &str) -> f64 {
        if self.precision.is_some() { js_parse_float(s) } else { js_parse_int(s) }
    }

    fn input(&self, state: &NumberState, submitted: bool, plain: bool) -> String {
        let t = &self.theme;
        if submitted || plain {
            return state.value.clone();
        }
        let annotation: Vec<&str> = if state.error.is_some() { vec![t.error_color] } else { vec![ansi::UNDERLINED, t.primary_color] };
        t.annotate(&state.value, &annotation)
    }

    fn output(&self, state: &NumberState, leading: &str, trailing: &str, plain: bool, submitted: bool) -> String {
        render_prompt(&self.input(state, submitted, plain), &self.message, leading, trailing, &self.theme, plain)
    }

    fn with_value(state: &NumberState, value: String) -> Action<NumberState, f64> {
        Action::NextFrame(NumberState { value, error: None, ..state.clone() })
    }

    /// `defaultIntProcessor` / `defaultFloatProcessor`.
    fn typed(&self, input: &str, state: &NumberState) -> Action<NumberState, f64> {
        if self.precision.is_some() && input == "." && state.value.contains('.') {
            return Action::Beep;
        }
        if state.value.is_empty() && input == "-" {
            return Number::with_value(state, "-".into());
        }
        let parsed = self.parse(&format!("{}{input}", state.value));
        if parsed.is_nan() {
            return Action::Beep;
        }
        let value = if self.precision.is_none() {
            js_number(parsed)
        } else if input == "." {
            format!("{}.", js_number(parsed))
        } else if state.value.contains('.') && input.len() == 1 && input.as_bytes()[0].is_ascii_digit() {
            format!("{}{input}", state.value)
        } else {
            js_number(parsed)
        };
        Number::with_value(state, value)
    }
}

impl Handlers for Number {
    type State = NumberState;
    type Output = f64;

    fn initial(&self) -> NumberState {
        let value = self.default.map(js_number).unwrap_or_default();
        NumberState { value, error: None }
    }

    fn render(&self, state: &NumberState, action: &Action<NumberState, f64>) -> String {
        let t = &self.theme;
        match action {
            Action::Beep => ansi::BEEP.into(),
            Action::NextFrame(state) => {
                let leading = t.annotate_symbol(t.prefix, &[t.primary_color]);
                let trailing = t.annotate_symbol(t.pointer_small, &[t.muted_color]);
                let error = render_error(state.error.as_deref(), t, false);
                self.output(state, &leading, &trailing, false, false) + &error
            }
            Action::Submit(_) => {
                let leading = t.annotate_symbol(t.tick, &[t.success_color]);
                let trailing = t.annotate_symbol(t.ellipsis, &[t.muted_color]);
                self.output(state, &leading, &trailing, false, true) + "\n"
            }
        }
    }

    fn process(&self, input: &UserInput, state: &NumberState) -> Action<NumberState, f64> {
        if input.ctrl && input.name == "u" {
            return Action::NextFrame(NumberState { value: String::new(), error: None });
        }
        let empty = state.value.is_empty() || state.value == "-";
        match input.name.as_str() {
            "backspace" => {
                if state.value.is_empty() {
                    return Action::Beep;
                }
                // `value.slice(0, length - 1)` on an ASCII number string.
                let mut value = state.value.clone();
                value.pop();
                Number::with_value(state, value)
            }
            "k" | "up" => Number::with_value(
                state,
                if empty { js_number(self.increment_by) } else { js_number(self.parse(&state.value) + self.increment_by) },
            ),
            "j" | "down" => Number::with_value(
                state,
                if empty { format!("-{}", js_number(self.decrement_by)) } else { js_number(self.parse(&state.value) - self.decrement_by) },
            ),
            "enter" | "return" => {
                let parsed = self.parse(&state.value);
                if parsed.is_nan() {
                    let error =
                        if self.precision.is_some() { "Must provide a floating point value" } else { "Must provide an integer value" };
                    return Action::NextFrame(NumberState { error: Some(error.into()), ..state.clone() });
                }
                let value = match self.precision {
                    Some(p) => {
                        let factor = 10f64.powi(p);
                        js_round(parsed * factor) / factor
                    }
                    None => parsed,
                };
                match self.check(value) {
                    Ok(v) => Action::Submit(v),
                    Err(error) => Action::NextFrame(NumberState { error: Some(error), ..state.clone() }),
                }
            }
            _ => self.typed(input.text(), state),
        }
    }

    fn clear(&self, state: &NumberState, _: &Action<NumberState, f64>, columns: usize) -> String {
        let t = &self.theme;
        let error = render_error(state.error.as_deref(), t, true);
        clear_output_with_error(&self.output(state, t.prefix, t.pointer_small, true, false), columns, &error) + &reset_line()
    }
}

// ---------------------------------------------------------------------------------------------
// Date (`Date` parameters, which ocd's commands do not have)

/// A JS `Date` in local time: milliseconds since the epoch, edited through the local-time setters.
#[cfg_attr(not(test), allow(dead_code))]
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct JsDate(pub f64);

/// Days from 1970-01-01 to `y-m-d` (proleptic Gregorian, `m` 1-based).
#[cfg_attr(not(test), allow(dead_code))]
fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = (m + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146097 + doe - 719468
}

#[cfg_attr(not(test), allow(dead_code))]
fn civil_from_days(z: i64) -> (i64, i64, i64) {
    let z = z + 719468;
    let era = z.div_euclid(146097);
    let doe = z - era * 146097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    (if m <= 2 { y + 1 } else { y }, m, d)
}

/// Local-time fields of a [`JsDate`]: year, month (0-based), day, hours, minutes, seconds, ms, weekday.
#[cfg_attr(not(test), allow(dead_code))]
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Fields {
    pub year: f64,
    pub month: f64,
    pub date: f64,
    pub hours: f64,
    pub minutes: f64,
    pub seconds: f64,
    pub ms: f64,
}

#[cfg_attr(not(test), allow(dead_code))]
const MS_PER_DAY: f64 = 86_400_000.0;

#[cfg_attr(not(test), allow(dead_code))]
impl JsDate {
    pub fn now() -> JsDate {
        JsDate(chrono::Utc::now().timestamp_millis() as f64)
    }

    fn offset_ms_at_utc(t: f64) -> f64 {
        use chrono::{Offset, TimeZone};
        match chrono::DateTime::from_timestamp_millis(t as i64) {
            Some(dt) => chrono::Local.offset_from_utc_datetime(&dt.naive_utc()).fix().local_minus_utc() as f64 * 1000.0,
            None => 0.0,
        }
    }

    fn local(self) -> f64 {
        self.0 + JsDate::offset_ms_at_utc(self.0)
    }

    /// `UTC(t)`: local time value to UTC, the offset taken at the local instant (a gap moves forward).
    fn from_local(t: f64) -> JsDate {
        if !t.is_finite() {
            return JsDate(f64::NAN);
        }
        let guess = t - JsDate::offset_ms_at_utc(t);
        JsDate(t - JsDate::offset_ms_at_utc(guess))
    }

    pub fn fields(self) -> Fields {
        let t = self.local();
        let days = (t / MS_PER_DAY).floor();
        let ms_in_day = t - days * MS_PER_DAY;
        let (y, m, d) = civil_from_days(days as i64);
        Fields {
            year: y as f64,
            month: (m - 1) as f64,
            date: d as f64,
            hours: (ms_in_day / 3_600_000.0).floor(),
            minutes: ((ms_in_day / 60_000.0).floor()) % 60.0,
            seconds: ((ms_in_day / 1000.0).floor()) % 60.0,
            ms: ms_in_day % 1000.0,
        }
    }

    /// `getDay()`.
    pub fn weekday(self) -> usize {
        let days = (self.local() / MS_PER_DAY).floor() as i64;
        (days + 4).rem_euclid(7) as usize
    }

    /// `MakeDate(MakeDay(year, month, date), MakeTime(...))` in local time.
    pub fn make(f: Fields) -> JsDate {
        let values = [f.year, f.month, f.date, f.hours, f.minutes, f.seconds, f.ms];
        if values.iter().any(|v| !v.is_finite()) {
            return JsDate(f64::NAN);
        }
        let y = f.year.trunc() + (f.month.trunc() / 12.0).floor();
        let m = f.month.trunc().rem_euclid(12.0);
        let day = days_from_civil(y as i64, m as i64 + 1, 1) as f64 + f.date.trunc() - 1.0;
        let time = f.hours.trunc() * 3_600_000.0 + f.minutes.trunc() * 60_000.0 + f.seconds.trunc() * 1000.0 + f.ms.trunc();
        JsDate::from_local(day * MS_PER_DAY + time)
    }

    fn set(self, change: impl FnOnce(&mut Fields)) -> JsDate {
        if self.0.is_nan() {
            return self;
        }
        let mut f = self.fields();
        change(&mut f);
        JsDate::make(f)
    }

    /// `toISOString()`.
    pub fn to_iso_string(self) -> String {
        let t = self.0;
        let days = (t / MS_PER_DAY).floor();
        let ms_in_day = t - days * MS_PER_DAY;
        let (y, m, d) = civil_from_days(days as i64);
        let year = if (0..=9999).contains(&y) {
            format!("{y:04}")
        } else if y < 0 {
            format!("-{:06}", -y)
        } else {
            format!("+{y:06}")
        };
        format!(
            "{year}-{m:02}-{d:02}T{:02}:{:02}:{:02}.{:03}Z",
            (ms_in_day / 3_600_000.0) as i64,
            ((ms_in_day / 60_000.0) as i64) % 60,
            ((ms_in_day / 1000.0) as i64) % 60,
            (ms_in_day as i64) % 1000
        )
    }
}

#[cfg_attr(not(test), allow(dead_code))]
#[derive(Clone, Debug, PartialEq)]
enum PartKind {
    Token,
    Milliseconds,
    Seconds,
    Minutes,
    Hours,
    Day,
    Month,
    Year,
    Meridiem,
}

#[cfg_attr(not(test), allow(dead_code))]
#[derive(Clone, Debug, PartialEq)]
struct DatePart {
    kind: PartKind,
    token: String,
}

#[cfg_attr(not(test), allow(dead_code))]
pub struct Locales {
    pub months: [&'static str; 12],
    pub months_short: [&'static str; 12],
    pub weekdays: [&'static str; 7],
    pub weekdays_short: [&'static str; 7],
}

#[cfg_attr(not(test), allow(dead_code))]
const DEFAULT_LOCALES: Locales = Locales {
    months: ["January", "February", "March", "April", "May", "June", "July", "August", "September", "October", "November", "December"],
    months_short: ["Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec"],
    weekdays: ["Sunday", "Monday", "Tuesday", "Wednesday", "Thursday", "Friday", "Saturday"],
    weekdays_short: ["Sun", "Mon", "Tue", "Wed", "Thu", "Fri", "Sat"],
};

#[cfg_attr(not(test), allow(dead_code))]
fn int_tail(value: &str, n: usize) -> f64 {
    let units: Vec<u16> = value.encode_utf16().collect();
    let tail = String::from_utf16_lossy(&units[units.len().saturating_sub(n)..]);
    js_parse_int(&tail)
}

#[cfg_attr(not(test), allow(dead_code))]
impl DatePart {
    fn is_token(&self) -> bool {
        self.kind == PartKind::Token
    }

    fn step(&self, date: JsDate, by: f64) -> JsDate {
        match self.kind {
            PartKind::Token => date,
            PartKind::Milliseconds => date.set(|f| f.ms += by),
            PartKind::Seconds => date.set(|f| f.seconds += by),
            PartKind::Minutes => date.set(|f| f.minutes += by),
            PartKind::Hours => date.set(|f| f.hours += by),
            PartKind::Day => date.set(|f| f.date += by),
            PartKind::Month => date.set(|f| f.month += by),
            PartKind::Year => date.set(|f| f.year += by),
            // increment and decrement both add 12 hours
            PartKind::Meridiem => date.set(|f| f.hours = (f.hours + 12.0) % 24.0),
        }
    }

    /// `setValue` (a token appends the value to itself).
    fn set_value(&mut self, date: JsDate, value: &str) -> JsDate {
        match self.kind {
            PartKind::Token => {
                self.token.push_str(value);
                date
            }
            PartKind::Milliseconds => {
                let v = int_tail(value, js_len(&self.token));
                date.set(|f| f.ms = v)
            }
            PartKind::Seconds => date.set(|f| f.seconds = int_tail(value, 2)),
            PartKind::Minutes => date.set(|f| f.minutes = int_tail(value, 2)),
            PartKind::Hours => date.set(|f| f.hours = int_tail(value, 2)),
            PartKind::Day => date.set(|f| f.date = int_tail(value, 2)),
            PartKind::Month => {
                let month = int_tail(value, 2) - 1.0;
                date.set(|f| f.month = if month < 0.0 { 0.0 } else { month })
            }
            PartKind::Year => date.set(|f| f.year = int_tail(value, 4)),
            PartKind::Meridiem => date,
        }
    }

    fn render(&self, date: JsDate) -> String {
        if self.kind == PartKind::Token {
            return self.token.clone();
        }
        if date.0.is_nan() {
            return "NaN".into();
        }
        let f = date.fields();
        let pad = |n: f64, w: usize| format!("{:0>w$}", js_number(n));
        let len = self.token.len();
        match self.kind {
            PartKind::Token => unreachable!(),
            PartKind::Milliseconds => {
                let s = pad(f.ms, 4);
                s[..len.min(s.len())].to_string()
            }
            PartKind::Seconds => {
                if len > 1 {
                    pad(f.seconds, 2)
                } else {
                    js_number(f.seconds)
                }
            }
            PartKind::Minutes => {
                if len > 1 {
                    pad(f.minutes, 2)
                } else {
                    js_number(f.minutes)
                }
            }
            PartKind::Hours => {
                let hours = if self.token.contains('h') { if f.hours % 12.0 == 0.0 { 12.0 } else { f.hours % 12.0 } } else { f.hours };
                if len > 1 { pad(hours, 2) } else { js_number(hours) }
            }
            PartKind::Day => {
                let d = f.date;
                match self.token.as_str() {
                    "DD" => pad(d, 2),
                    "Do" => {
                        let n = d as i64;
                        let suffix = if (11..=13).contains(&n) {
                            "th"
                        } else {
                            match n % 10 {
                                1 => "st",
                                2 => "nd",
                                3 => "rd",
                                _ => "th",
                            }
                        };
                        format!("{n}{suffix}")
                    }
                    "d" => (date.weekday() + 1).to_string(),
                    "ddd" => DEFAULT_LOCALES.weekdays_short[date.weekday()].into(),
                    "dddd" => DEFAULT_LOCALES.weekdays[date.weekday()].into(),
                    _ => js_number(d),
                }
            }
            PartKind::Month => match len {
                2 => pad(f.month + 1.0, 2),
                3 => DEFAULT_LOCALES.months_short[f.month as usize].into(),
                4 => DEFAULT_LOCALES.months[f.month as usize].into(),
                _ => js_number(f.month + 1.0),
            },
            PartKind::Year => {
                let year = pad(f.year, 4);
                if len == 2 { year[year.len() - 2..].to_string() } else { year }
            }
            PartKind::Meridiem => {
                let m = if f.hours >= 12.0 { "pm" } else { "am" };
                if self.token.contains('A') { m.to_uppercase() } else { m.into() }
            }
        }
    }
}

/// `makeDateParts`: the mask split into editable parts and literal tokens (adjacent tokens merged).
#[cfg_attr(not(test), allow(dead_code))]
fn make_date_parts(mask: &str) -> Vec<DatePart> {
    static RE: std::sync::LazyLock<regex::Regex> = std::sync::LazyLock::new(|| {
        regex::Regex::new(
            r#"\\(.)|"((?:\\["\\]|[^"])+)"|(D[Do]?|d{3,4}|d)|(M{1,4})|(YY(?:YY)?)|([aA])|([Hh]{1,2})|(m{1,2})|(s{1,2})|(S{1,4})|."#,
        )
        .unwrap()
    });
    static UNESCAPE: std::sync::LazyLock<regex::Regex> = std::sync::LazyLock::new(|| regex::Regex::new(r"\\(.)").unwrap());
    let mut parts: Vec<DatePart> = vec![];
    for caps in RE.captures_iter(mask) {
        let whole = caps.get(0).unwrap().as_str();
        let group = (1..=10).find(|&i| caps.get(i).is_some());
        let part = match group {
            Some(2) => DatePart { kind: PartKind::Token, token: UNESCAPE.replace_all(caps.get(2).unwrap().as_str(), "$1").into_owned() },
            Some(g @ 3..=10) => {
                let kind = match g {
                    3 => PartKind::Day,
                    4 => PartKind::Month,
                    5 => PartKind::Year,
                    6 => PartKind::Meridiem,
                    7 => PartKind::Hours,
                    8 => PartKind::Minutes,
                    9 => PartKind::Seconds,
                    _ => PartKind::Milliseconds,
                };
                DatePart { kind, token: caps.get(g).unwrap().as_str().to_string() }
            }
            Some(1) => DatePart { kind: PartKind::Token, token: caps.get(1).unwrap().as_str().to_string() },
            _ => DatePart { kind: PartKind::Token, token: whole.to_string() },
        };
        match parts.last_mut() {
            Some(last) if part.is_token() && last.is_token() => last.token.push_str(&part.token),
            _ => parts.push(part),
        }
    }
    parts
}

#[cfg_attr(not(test), allow(dead_code))]
pub type DateValidate = Box<dyn Fn(JsDate) -> Result<JsDate, String>>;

#[cfg_attr(not(test), allow(dead_code))]
pub struct DatePrompt {
    pub message: String,
    pub initial: JsDate,
    pub mask: String,
    pub validate: Option<DateValidate>,
    pub theme: Theme,
}

#[cfg_attr(not(test), allow(dead_code))]
#[derive(Clone, Debug)]
pub struct DateState {
    typed: String,
    cursor: usize,
    value: JsDate,
    parts: Vec<DatePart>,
    error: Option<String>,
}

#[cfg_attr(not(test), allow(dead_code))]
impl DatePrompt {
    /// `Prompt.Date({ message })`: now, `YYYY-MM-DD HH:mm:ss`.
    pub fn new(message: &str, theme: &Theme) -> DatePrompt {
        DatePrompt {
            message: message.into(),
            initial: JsDate::now(),
            mask: "YYYY-MM-DD HH:mm:ss".into(),
            validate: None,
            theme: theme.clone(),
        }
    }

    fn render_parts(&self, state: &DateState, submitted: bool) -> String {
        let t = &self.theme;
        let mut doc = String::new();
        for (i, part) in state.parts.iter().enumerate() {
            let text = part.render(state.value);
            if i == state.cursor && !submitted {
                doc.push_str(&t.annotate(&text, &[ansi::UNDERLINED, t.primary_color]));
            } else {
                doc.push_str(&text);
            }
        }
        doc
    }

    fn next_part(parts: &[DatePart], cursor: usize) -> Option<usize> {
        (cursor + 1..parts.len()).find(|&i| !parts[i].is_token())
    }

    fn previous_part(parts: &[DatePart], cursor: usize) -> Option<usize> {
        (0..cursor).rev().find(|&i| !parts[i].is_token())
    }
}

#[cfg_attr(not(test), allow(dead_code))]
impl Handlers for DatePrompt {
    type State = DateState;
    type Output = JsDate;

    fn initial(&self) -> DateState {
        let parts = make_date_parts(&self.mask);
        let cursor = parts.iter().position(|p| !p.is_token()).unwrap_or(usize::MAX);
        DateState { typed: String::new(), cursor, value: self.initial, parts, error: None }
    }

    fn render(&self, state: &DateState, action: &Action<DateState, JsDate>) -> String {
        let t = &self.theme;
        match action {
            Action::Beep => ansi::BEEP.into(),
            Action::NextFrame(state) => {
                let leading = t.annotate_symbol(t.prefix, &[t.primary_color]);
                let trailing = t.annotate_symbol(t.pointer_small, &[t.muted_color]);
                let parts = self.render_parts(state, false);
                let prompt = render_prompt(&parts, &self.message, &leading, &trailing, t, false);
                format!("{}{prompt}{}", ansi::CURSOR_HIDE, render_error(state.error.as_deref(), t, false))
            }
            Action::Submit(_) => {
                let leading = t.annotate_symbol(t.tick, &[t.success_color]);
                let trailing = t.annotate_symbol(t.ellipsis, &[t.muted_color]);
                let parts = self.render_parts(state, true);
                render_prompt(&parts, &self.message, &leading, &trailing, t, false) + "\n"
            }
        }
    }

    fn process(&self, input: &UserInput, state: &DateState) -> Action<DateState, JsDate> {
        let Some(part) = state.parts.get(state.cursor) else { return Action::Beep };
        match input.name.as_str() {
            "left" => match DatePrompt::previous_part(&state.parts, state.cursor) {
                Some(cursor) => Action::NextFrame(DateState { typed: String::new(), cursor, ..state.clone() }),
                None => Action::Beep,
            },
            "right" => match DatePrompt::next_part(&state.parts, state.cursor) {
                Some(cursor) => Action::NextFrame(DateState { typed: String::new(), cursor, ..state.clone() }),
                None => Action::Beep,
            },
            "k" | "up" => Action::NextFrame(DateState { typed: String::new(), value: part.step(state.value, 1.0), ..state.clone() }),
            "j" | "down" => Action::NextFrame(DateState { typed: String::new(), value: part.step(state.value, -1.0), ..state.clone() }),
            "tab" => {
                let cursor = DatePrompt::next_part(&state.parts, state.cursor)
                    .unwrap_or_else(|| state.parts.iter().position(|p| !p.is_token()).unwrap_or(usize::MAX));
                Action::NextFrame(DateState { typed: String::new(), cursor, ..state.clone() })
            }
            "enter" | "return" => {
                let checked = match &self.validate {
                    Some(v) => v(state.value),
                    None => Ok(state.value),
                };
                match checked {
                    Ok(value) => Action::Submit(value),
                    Err(error) => Action::NextFrame(DateState { error: Some(error), ..state.clone() }),
                }
            }
            _ => {
                let value = input.text();
                if !value.chars().any(|c| c.is_ascii_digit()) {
                    return Action::Beep;
                }
                let typed = format!("{}{value}", state.typed);
                let mut parts = state.parts.clone();
                let date = parts[state.cursor].set_value(state.value, &typed);
                Action::NextFrame(DateState { typed, value: date, parts, ..state.clone() })
            }
        }
    }

    fn clear(&self, state: &DateState, action: &Action<DateState, JsDate>, columns: usize) -> String {
        let t = &self.theme;
        let value = match action {
            Action::NextFrame(next) => next.value,
            _ => state.value,
        };
        let parts: String = state.parts.iter().map(|p| p.render(value)).collect();
        let text = render_prompt(&parts, &self.message, t.prefix, t.pointer_small, t, true);
        let error = match &state.error {
            Some(e) => separate_symbol(t.pointer_small, &split_lines(e).join("\n")),
            None => String::new(),
        };
        clear_output_with_error(&text, columns, &error) + &reset_line()
    }
}

// ---------------------------------------------------------------------------------------------
// terminals

/// `NodeTerminal` on the process's stdin/stdout: keypresses as node's `readline` parses them (50 ms
/// escape timeout), raw mode while the wizard reads keys, the end of input ending every later prompt.
pub struct TtyTerminal {
    buf: Vec<u8>,
    queue: VecDeque<UserInput>,
    /// The current prompt's queue was ended by a quit key.
    done: bool,
    /// stdin reached its end (`inputEnded`).
    eof: bool,
    #[cfg(unix)]
    raw: Option<crate::ui::RawMode>,
}

impl Default for TtyTerminal {
    fn default() -> Self {
        TtyTerminal::new()
    }
}

impl TtyTerminal {
    pub fn new() -> TtyTerminal {
        TtyTerminal {
            buf: vec![],
            queue: VecDeque::new(),
            done: false,
            eof: false,
            #[cfg(unix)]
            raw: None,
        }
    }

    /// Leaves raw mode (the `RcRef` idle release once no prompt reads keys).
    pub fn release(&mut self) {
        #[cfg(unix)]
        {
            self.raw = None;
        }
    }

    fn fill(&mut self) {
        use std::io::Read;
        loop {
            if !crate::ui::wait_input() {
                continue;
            }
            let mut chunk = [0u8; 256];
            match std::io::stdin().read(&mut chunk) {
                Ok(0) | Err(_) => {
                    self.eof = true;
                    return;
                }
                Ok(n) => {
                    self.buf.extend_from_slice(&chunk[..n]);
                    let mut keys = crate::clack::parse_keys(&mut self.buf, false);
                    if !self.buf.is_empty() && !crate::ui::input_within(50) {
                        keys.extend(crate::clack::parse_keys(&mut self.buf, true));
                    }
                    for key in keys {
                        let input = UserInput::from_key(&key);
                        let quit = should_quit(&input);
                        self.queue.push_back(input);
                        if quit {
                            // `Queue.endUnsafe`: later keys of the chunk are dropped.
                            self.done = true;
                            break;
                        }
                    }
                    if self.done || !self.queue.is_empty() {
                        return;
                    }
                }
            }
        }
    }
}

impl Terminal for TtyTerminal {
    fn columns(&mut self) -> usize {
        crate::term::columns().unwrap_or(0)
    }

    fn begin(&mut self) {
        self.queue.clear();
        self.done = false;
        #[cfg(unix)]
        if !self.eof && self.raw.is_none() && crate::term::stdin_is_tty() {
            self.raw = crate::ui::RawMode::enter();
        }
    }

    fn read_input(&mut self) -> Option<UserInput> {
        loop {
            if let Some(input) = self.queue.pop_front() {
                return Some(input);
            }
            if self.done || self.eof {
                return None;
            }
            self.fill();
        }
    }

    fn display(&mut self, s: &str) {
        crate::term::write_stdout(s);
    }
}

/// A scripted terminal for tests: each chunk is what one read of the terminal returns, parsed into
/// keypresses like the TTY's; keys of a chunk left when a prompt ends are gone, like [`TtyTerminal`]'s.
#[cfg(test)]
pub struct ScriptTerminal {
    chunks: VecDeque<Vec<UserInput>>,
    queue: VecDeque<UserInput>,
    done: bool,
    pub columns: usize,
    pub out: String,
}

#[cfg(test)]
impl ScriptTerminal {
    /// Chunks as the bytes a terminal sends (`"\r"`, `"\x1b[B"`, `"\x03"`, ...).
    pub fn new<S: AsRef<str>>(chunks: &[S], columns: usize) -> ScriptTerminal {
        let chunks = chunks
            .iter()
            .map(|chunk| {
                let mut buf = chunk.as_ref().as_bytes().to_vec();
                crate::clack::parse_keys(&mut buf, true).iter().map(UserInput::from_key).collect()
            })
            .collect();
        ScriptTerminal { chunks, queue: VecDeque::new(), done: false, columns, out: String::new() }
    }
}

/// Expected prompt text, written with `defaultTheme`'s figures, as this platform's theme draws it: `windowsTheme`'s
/// figures on Windows.
#[cfg(test)]
pub fn platform_figures(text: &str) -> String {
    if !cfg!(windows) {
        return text.to_string();
    }
    text.replace('❯', ">").replace('›', "»").replace('…', "...").replace('✔', "√")
}

#[cfg(test)]
impl Terminal for ScriptTerminal {
    fn columns(&mut self) -> usize {
        self.columns
    }
    fn begin(&mut self) {
        self.queue.clear();
        self.done = false;
    }
    fn read_input(&mut self) -> Option<UserInput> {
        loop {
            if let Some(input) = self.queue.pop_front() {
                return Some(input);
            }
            if self.done {
                return None;
            }
            for input in self.chunks.pop_front()? {
                let quit = should_quit(&input);
                self.queue.push_back(input);
                if quit {
                    self.done = true;
                    break;
                }
            }
        }
    }
    fn display(&mut self, s: &str) {
        self.out.push_str(s);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn plain() -> Theme {
        Theme::new(false)
    }

    fn w(text: &str) -> String {
        platform_figures(text)
    }

    #[test]
    fn js_numbers() {
        assert_eq!(js_parse_int("12a"), 12.0);
        assert!(js_parse_int("-").is_nan());
        assert_eq!(js_parse_int("0x1f"), 31.0);
        assert_eq!(js_number(js_parse_int("-0")), "0");
        assert_eq!(js_number(js_parse_int("1234567890123456789012")), "1.2345678901234568e+21");
        assert_eq!(js_parse_int("1e+215"), 1.0);
        assert_eq!(js_parse_float("1.5e3x"), 1500.0);
        assert_eq!(js_parse_float("-.5"), -0.5);
        assert!(js_parse_float(".").is_nan());
        assert_eq!(js_round(-2.5), -2.0);
        assert_eq!(js_round(2.5), 3.0);
    }

    #[test]
    fn confirm_frames() {
        let mut term = ScriptTerminal::new(&["x", "y"], 80);
        let value = run(&Confirm::new("Continue?", false, &Theme::new(true)), &mut term).unwrap();
        assert!(value);
        assert_eq!(
            term.out,
            w("\x1b[?25l\x1b[96m?\x1b[0m \x1b[1mContinue?\x1b[0m \x1b[90m›\x1b[0m \x1b[90m(y/N)\x1b[0m\x07\
             \x1b[2K\x1b[G\x1b[2K\x1b[G\x1b[32m✔\x1b[0m \x1b[1mContinue?\x1b[0m \x1b[90m…\x1b[0m yes\n\x1b[?25h")
        );
    }

    #[test]
    fn confirm_regexes_and_enter() {
        for (keys, expected) in [("\r", false), ("T", true), ("f", false), ("N", false)] {
            let mut term = ScriptTerminal::new(&[keys], 80);
            assert_eq!(run(&Confirm::new("Q", false, &plain()), &mut term), Ok(expected), "{keys:?}");
        }
        let mut term = ScriptTerminal::new(&["\x03"], 80);
        assert_eq!(run(&Confirm::new("Q", true, &plain()), &mut term), Err(Quit));
        assert_eq!(term.out, w("\x1b[?25l? Q › (Y/n)\x07\x1b[?25h"));
    }

    #[test]
    fn text_editing_and_validation() {
        let mut prompt = Text::new("Name", TextKind::Text, &plain());
        prompt.validate = Box::new(|v| if v.is_empty() { Err("required\nnow".into()) } else { Ok(v.to_string()) });
        let mut term = ScriptTerminal::new(&["\r", "ab", "\x1b[D", "c", "\x01", "\x15", "z", "\r"], 20);
        assert_eq!(run(&prompt, &mut term), Ok("z".into()));
        let frames: Vec<&str> = term.out.split("\x1b[2K\x1b[G").collect();
        assert_eq!(frames[0], w("? Name › "));
        // the error frame: saved cursor, the error lines, restored cursor
        assert!(frames[2].starts_with(&w("? Name › \x1b[s\n› required\nnow\x1b[u")));
        // clearing it moves down over the error, erases it and the prompt line
        assert!(term.out.contains("\x1b[2B\x1b[2K\x1b[1A\x1b[2K\x1b[1A\x1b[2K\x1b[G"));
        assert!(term.out.contains(&w("? Name › acb\x1b[1D")));
        assert!(term.out.ends_with(&w("✔ Name … z\n\x1b[?25h")));
    }

    #[test]
    fn hidden_shows_nothing() {
        let mut term = ScriptTerminal::new(&["abc", "\r"], 80);
        assert_eq!(run(&Text::new("Token", TextKind::Hidden, &plain()), &mut term), Ok("abc".into()));
        assert!(term.out.starts_with(&w("? Token › \x1b[2K\x1b[G\x1b[2K\x1b[G? Token › \x1b[2K")));
        assert!(term.out.ends_with(&w("✔ Token … \n\x1b[?25h")));
    }

    #[test]
    fn password_masks_and_tab_default() {
        let mut prompt = Text::new("Secret", TextKind::Password, &plain());
        prompt.default = "pw".into();
        let mut term = ScriptTerminal::new(&["\x15", "\t", "x\r"], 80);
        assert_eq!(run(&prompt, &mut term), Ok("pwx".into()));
        assert!(term.out.starts_with(&w("? Secret › **")));
        assert!(term.out.ends_with(&w("✔ Secret … ***\n\x1b[?25h")));
    }

    #[test]
    fn int_prompt() {
        let mut prompt = Number::int("Count", &plain());
        prompt.default = Some(1.0);
        prompt.min = 1.0;
        prompt.max = 2.0;
        let mut term = ScriptTerminal::new(&["k", "k", "\r", "\x7f", "a", "-", "\r", "\x15", "2", "\r"], 80);
        assert_eq!(run(&prompt, &mut term), Ok(2.0));
        assert!(term.out.contains(&w("? Count › 3\x1b[s\n› 3 must be less than or equal to 2\x1b[u")));
        // "a" beeps (NaN), "-" starts a negative number, which alone is no integer
        assert!(term.out.contains(&w("? Count › -\x1b[s\n› Must provide an integer value\x1b[u")));
        assert!(term.out.contains(&w("? Count › -\x1b[2K")));
    }

    #[test]
    fn number_prompt_rounds() {
        let mut term = ScriptTerminal::new(&["1", ".", ".", "2", "5", "6", "\r"], 80);
        assert_eq!(run(&Number::float("N", &plain()), &mut term), Ok(1.26));
        assert!(term.out.contains(&w("? N › 1.\x07")));
        assert!(term.out.ends_with(&w("✔ N … 1.256\n\x1b[?25h")));
    }

    #[test]
    fn select_pages_and_wraps() {
        let choices: Vec<Choice> =
            (0..12).map(|i| Choice { title: format!("c{i}"), description: Some(format!("d{i}")), disabled: false }).collect();
        let mut term = ScriptTerminal::new(&["\x1b[A", "j", "\r"], 80);
        assert_eq!(run(&Select::new(Some("Pick"), choices, &plain()), &mut term), Ok(0));
        let first: Vec<&str> = term.out.split("\x1b[2K").next().unwrap().lines().collect();
        assert_eq!(first[0], w("\x1b[?25l? Pick › "));
        assert_eq!(first[1], w("❯ c0 - d0"));
        assert_eq!(first[10], "↓ c9 ");
        // the cursor on the last choice shows the last page with an up arrow
        assert!(term.out.contains(&w("? Pick › \n↑ c2 \n")));
        assert!(term.out.contains(&w("❯ c11 - d11")));
        assert!(term.out.ends_with(&w("✔ Pick …  c0\n\x1b[?25h")));
    }

    #[test]
    fn toggle_prompt() {
        let mut term = ScriptTerminal::new(&["\x1b[C", " ", "\t", "\r"], 80);
        let toggle = Toggle { message: "Run?".into(), initial: true, active: "yes".into(), inactive: "no".into(), theme: Theme::new(true) };
        assert_eq!(run(&toggle, &mut term), Ok(true));
        assert!(term.out.contains("\x1b[4m\x1b[96mno\x1b[0m\x07"));
        assert!(term.out.ends_with("\x1b[4m\x1b[37myes\x1b[0m \x1b[90m/\x1b[0m no\n\x1b[?25h"));
    }

    #[test]
    fn erase_measures_wrapped_rows() {
        assert_eq!(erase_text("abcdef\nxy", 3), ansi::erase_lines(3));
        assert_eq!(erase_text("abc", 0), "\x1b[2K\x1b[1G");
        assert_eq!(count_lines("abcd\n", 3), 2);
    }

    #[test]
    fn date_parts_and_editing() {
        let parts = make_date_parts("YYYY-MM-DD HH:mm:ss");
        let kinds: Vec<String> = parts.iter().map(|p| p.token.clone()).collect();
        assert_eq!(kinds, ["YYYY", "-", "MM", "-", "DD", " ", "HH", ":", "mm", ":", "ss"]);
        let merged = make_date_parts(r#"\a"b c"Do"#);
        assert_eq!(merged.iter().map(|p| p.token.as_str()).collect::<Vec<_>>(), ["ab c", "Do"]);
        let start = JsDate::make(Fields { year: 2024.0, month: 0.0, date: 31.0, hours: 23.0, minutes: 5.0, seconds: 0.0, ms: 0.0 });
        let mut prompt = DatePrompt::new("When", &plain());
        prompt.initial = start;
        // year: type 2025; tab to month, up (Jan 31 -> Mar 3 2025 by JS overflow), right to day, type
        // 0 (day 0 of March is Feb 28) and 7: Feb 7
        let mut term = ScriptTerminal::new(&["2", "0", "2", "5", "\t", "\x1b[A", "\x1b[C", "0", "7", "x", "\r"], 80);
        let date = run(&prompt, &mut term).unwrap();
        let f = date.fields();
        assert_eq!((f.year, f.month, f.date, f.hours, f.minutes), (2025.0, 1.0, 7.0, 23.0, 5.0));
        assert!(term.out.contains(&w("? When › 2025-03-03 23:05:00")));
        assert!(term.out.ends_with(&w("✔ When … 2025-02-07 23:05:00\n\x1b[?25h")));
        assert_eq!(JsDate(1_700_000_000_123.0).to_iso_string(), "2023-11-14T22:13:20.123Z");
        assert!(term.out.contains("\x07"));
    }
}
