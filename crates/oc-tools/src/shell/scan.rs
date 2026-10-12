//! Portable shell scanner (`core/src/shell/scan.ts`): a hand-written bash/zsh/posix and PowerShell
//! command scanner used for permission analysis. Line-by-line port of the upstream TypeScript.
//!
//! Indexing: upstream walks JavaScript strings by UTF-16 code unit. This port walks Unicode scalar
//! values (`char`s), which is identical for BMP text. `Command::word_ends` are byte offsets into
//! `Command::resource`.

#![allow(clippy::collapsible_if, clippy::if_same_then_else, clippy::enum_variant_names, clippy::too_many_arguments)]

use std::cell::RefCell;
use std::collections::HashSet;
use std::rc::Rc;
use std::sync::LazyLock;

use fancy_regex::Regex;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OpaqueReason {
    CommandSubstitution,
    CompoundCommand,
    DynamicCommandName,
    DynamicExecution,
    Heredoc,
    InvalidRedirect,
    InvalidStructure,
    UnterminatedEscape,
    UnterminatedQuote,
}

impl OpaqueReason {
    pub fn as_str(&self) -> &'static str {
        match self {
            OpaqueReason::CommandSubstitution => "command-substitution",
            OpaqueReason::CompoundCommand => "compound-command",
            OpaqueReason::DynamicCommandName => "dynamic-command-name",
            OpaqueReason::DynamicExecution => "dynamic-execution",
            OpaqueReason::Heredoc => "heredoc",
            OpaqueReason::InvalidRedirect => "invalid-redirect",
            OpaqueReason::InvalidStructure => "invalid-structure",
            OpaqueReason::UnterminatedEscape => "unterminated-escape",
            OpaqueReason::UnterminatedQuote => "unterminated-quote",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Command {
    pub resource: String,
    pub words: Vec<String>,
    pub raw_words: Vec<String>,
    /// Exclusive raw-token ends (byte offsets) relative to `resource`, for source-shaped permission prefixes.
    pub word_ends: Option<Vec<usize>>,
    pub statement_head: bool,
    pub declaration: bool,
    /// Words after a trailing redirect are destinations in the legacy command span.
    pub redirect_word_count: Option<usize>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ScanOutcome {
    Scanned(Vec<Command>),
    Opaque(OpaqueReason),
}

/// Bash and Zsh name those shells. Posix covers sh, dash, and any other shell.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Dialect {
    Bash,
    Zsh,
    Posix,
}

use OpaqueReason as O;

type R<T> = Result<T, OpaqueReason>;
type CmdRef = Rc<RefCell<Command>>;

// ---------------------------------------------------------------------------------------------
// Input helpers

pub(crate) struct Input {
    chars: Vec<char>,
    s: String,
    /// Byte offset of each char index, plus the total length.
    byte: Vec<usize>,
}

impl Input {
    pub(crate) fn new(s: &str) -> Self {
        let mut chars = Vec::with_capacity(s.len());
        let mut byte = Vec::with_capacity(s.len() + 1);
        for (offset, c) in s.char_indices() {
            chars.push(c);
            byte.push(offset);
        }
        byte.push(s.len());
        Input { chars, s: s.to_string(), byte }
    }
    fn len(&self) -> usize {
        self.chars.len()
    }
    fn at(&self, i: usize) -> Option<char> {
        self.chars.get(i).copied()
    }
    fn ati(&self, i: isize) -> Option<char> {
        if i < 0 { None } else { self.at(i as usize) }
    }
    fn is(&self, i: usize, c: char) -> bool {
        self.at(i) == Some(c)
    }
    /// `String.prototype.slice` with clamping.
    fn slice(&self, a: usize, b: usize) -> &str {
        let a = a.min(self.len());
        let b = b.min(self.len());
        if a >= b { "" } else { &self.s[self.byte[a]..self.byte[b]] }
    }
    fn slice_from(&self, a: usize) -> &str {
        self.slice(a, self.len())
    }
    fn starts_with(&self, pat: &str, at: usize) -> bool {
        if at > self.len() {
            return pat.is_empty();
        }
        self.s[self.byte[at]..].starts_with(pat)
    }
    fn char_index_of_byte(&self, b: usize) -> usize {
        match self.byte.binary_search(&b) {
            Ok(i) => i,
            Err(i) => i,
        }
    }
    fn index_of(&self, pat: &str, from: usize) -> Option<usize> {
        if from > self.len() {
            return if pat.is_empty() { Some(self.len()) } else { None };
        }
        let start = self.byte[from];
        self.s[start..].find(pat).map(|b| self.char_index_of_byte(start + b))
    }
    fn index_of_char(&self, c: char, from: usize) -> Option<usize> {
        (from..self.len()).find(|&i| self.chars[i] == c)
    }
    fn includes(&self, pat: &str, from: usize) -> bool {
        self.index_of(pat, from).is_some()
    }
    /// Sticky match: length in chars of the match of `re` (anchored with `^`) at char index `at`.
    fn sticky(&self, re: &Regex, at: usize) -> Option<usize> {
        if at > self.len() {
            return None;
        }
        let rest = &self.s[self.byte[at]..];
        match re.find(rest) {
            Ok(Some(m)) if m.start() == 0 => Some(rest[..m.end()].chars().count()),
            _ => None,
        }
    }
    fn sticky_caps(&self, re: &Regex, at: usize) -> Option<Vec<Option<String>>> {
        if at > self.len() {
            return None;
        }
        let rest = &self.s[self.byte[at]..];
        match re.captures(rest) {
            Ok(Some(c)) if c.get(0).map(|m| m.start()) == Some(0) => {
                Some((0..c.len()).map(|i| c.get(i).map(|m| m.as_str().to_string())).collect())
            }
            _ => None,
        }
    }
}

fn re(pattern: &str) -> Regex {
    Regex::new(pattern).expect("valid scanner regex")
}

/// Anchored ("sticky") regex.
fn sticky(pattern: &str) -> Regex {
    re(&format!("^(?:{pattern})"))
}

fn test(regex: &Regex, text: &str) -> bool {
    regex.is_match(text).unwrap_or(false)
}

/// JavaScript `\s`.
pub(crate) fn js_space(c: char) -> bool {
    matches!(
        c,
        '\t' | '\n' | '\u{0B}' | '\u{0C}' | '\r' | ' ' | '\u{A0}' | '\u{1680}' | '\u{2000}'
            ..='\u{200A}' | '\u{2028}' | '\u{2029}' | '\u{202F}' | '\u{205F}' | '\u{3000}' | '\u{FEFF}'
    )
}

/// JavaScript `String.prototype.trim`.
pub(crate) fn js_trim(s: &str) -> &str {
    s.trim_matches(js_space)
}

fn js_trim_end(s: &str) -> &str {
    s.trim_end_matches(js_space)
}

const JS_SPACE_CLASS: &str = r"\t\n\x0B\x0C\r \x{A0}\x{1680}\x{2000}-\x{200A}\x{2028}\x{2029}\x{202F}\x{205F}\x{3000}\x{FEFF}";

/// Expand JavaScript-specific escapes in a regex source: `\s`, `\S`, `\d`, `\w`.
fn js(pattern: &str) -> String {
    let mut out = String::with_capacity(pattern.len() + 16);
    let mut chars = pattern.chars().peekable();
    let mut in_class = false;
    while let Some(c) = chars.next() {
        if c == '\\' {
            let Some(n) = chars.next() else {
                out.push(c);
                break;
            };
            match n {
                's' if in_class => out.push_str(JS_SPACE_CLASS),
                's' => out.push_str(&format!("[{JS_SPACE_CLASS}]")),
                'S' if !in_class => out.push_str(&format!("[^{JS_SPACE_CLASS}]")),
                'd' if in_class => out.push_str("0-9"),
                'd' => out.push_str("[0-9]"),
                'w' if in_class => out.push_str("A-Za-z0-9_"),
                'w' => out.push_str("[A-Za-z0-9_]"),
                _ => {
                    out.push('\\');
                    out.push(n);
                }
            }
            continue;
        }
        if c == '[' && !in_class {
            in_class = true;
        } else if c == ']' && in_class {
            in_class = false;
        }
        out.push(c);
    }
    out
}

macro_rules! regex {
    ($name:ident, sticky $pat:expr) => {
        static $name: LazyLock<Regex> = LazyLock::new(|| sticky(&js($pat)));
    };
    ($name:ident, $pat:expr) => {
        static $name: LazyLock<Regex> = LazyLock::new(|| re(&js($pat)));
    };
}

fn utf16_len(s: &str) -> usize {
    s.chars().map(char::len_utf16).sum()
}

// ---------------------------------------------------------------------------------------------
// Bash constants

const BASH_REDIRECTS: &[&str] = &["&>>", "&>", "<<<", "<<-", "<<", "<>", "<&", ">&", ">|", ">>", ">", "<"];
const BASH_LIST_OPERATORS: &[&str] = &["&&", "||", "|&"];
const BASH_CASE_OPERATORS: &[&str] = &[";;&", ";;", ";&", ";|", "&&", "||", "|&"];
const BASH_BINDING_BUILTINS: &[&str] = &[
    "declare", "typeset", "local", "export", "readonly", "private", "unset", "read", "getln", "vared", "getopts",
    "mapfile", "readarray", "compgen", "zparseopts", "zregexparse", "zformat", "zstyle", "zstat", "stat",
];
const BASH_ARITHMETIC_COMPARISONS: &[&str] = &["-eq", "-ne", "-lt", "-le", "-gt", "-ge"];
const BASH_ASSIGNMENT_DECLARATIONS: &[&str] = &["declare", "typeset", "export", "readonly", "local"];
const BASH_DECLARATIONS: &[&str] = &["declare", "typeset", "export", "readonly", "local", "unset", "unsetenv"];
const BASH_PRECOMMANDS: &[&str] = &["builtin", "command", "time", "coproc", "!"];
const ZSH_PRECOMMANDS: &[&str] = &["noglob", "nocorrect", "exec", "-"];
const ZSH_ARITHMETIC_BUILTINS: &[&str] =
    &["integer", "float", "exit", "return", "shift", "break", "continue", "bye", "logout"];
const BASH_NON_FUNCTION_KEYWORDS: &[&str] =
    &["if", "for", "select", "case", "then", "elif", "else", "fi", "do", "done", "in", "esac"];
const BASH_UNIT_STARTS: &str = "\\$'\"`<>";
const MAX_INPUT_LENGTH: usize = 64 * 1024;
const MAX_SUBSTITUTION_DEPTH: usize = 32;

fn has(set: &[&str], value: &str) -> bool {
    set.contains(&value)
}

regex!(BASH_EXPANSION_RE, r"\$(?:\\[^\n\r\x{2028}\x{2029}]|\\\n)*[({]|`");
regex!(BASH_ESCAPE_RE, r"\\(?:x[\da-fA-F]{1,2}|[0-7]{1,3}|u[\da-fA-F]{1,4}|U[\da-fA-F]{1,8})");
regex!(TOKEN_RE, sticky r"[A-Za-z_][A-Za-z0-9_]*(?=(?:\\\n)*(?:[ \t\n;|&()<>]|$))");
regex!(BRACE_CLOSE_AHEAD_RE, sticky r"(?:\\\n)*(?:[ \t\n;&|()<>]|$)");
regex!(SPACE_CONTINUATION_AHEAD_RE, sticky r"(?:\\\n)*[ \t\n]");
regex!(NEGATION_AHEAD_RE, sticky r"(?:\\\n)*[ \t\n(]");
regex!(
    COPROC_AHEAD_RE,
    sticky r"coproc(?:[ \t]|\\\n)+(?:--(?:[ \t]|\\\n)+)?(?:[A-Za-z_][A-Za-z0-9_]*(?:[ \t]|\\\n)+)?(?=(?:!(?:[ \t]|\\\n)*)*(?:[{(]|\[\[(?:\\\n)*[ \t\n]|(?:if|while|until|for|select|case|repeat)(?![A-Za-z0-9_])))"
);
regex!(
    TIME_AHEAD_RE,
    sticky r"time(?:[ \t]|\\\n)+(?:-p(?:[ \t]|\\\n)+)?(?:--(?:[ \t]|\\\n)+)?(?=(?:!(?:[ \t]|\\\n)*)*(?:[{(]|\[\[(?:\\\n)*[ \t\n]|(?:if|while|until|for|select|case|repeat)(?![A-Za-z0-9_])))"
);
regex!(
    FUNCTION_HEAD_RE,
    sticky r"(function[ \t](?:[ \t]|\\\n)*)?([A-Za-z_][\w.:+@%-]*)?(?:[ \t]|\\\n)*(\([ \t]*\))?((?:[ \t\n]|\\\n|#[^\n]*\n)*)"
);
regex!(
    FUNCTION_BODY_RE,
    sticky r"[{(]|\[\[(?=(?:\\\n)*[ \t\n])|(?:if|while|until|for|select|case|repeat)(?=(?:\\\n)*(?:[ \t\n(]|$))"
);
regex!(
    DO_AHEAD_RE,
    sticky r"(?:[ \t\n;]|\\\n|#[^\n]*(?:\n|$))*(?:do(?=(?:\\\n)*(?:[ \t\n;{(]|$))|\{(?=(?:\\\n)*[ \t\n]))"
);
regex!(NOFORK_OPEN_RE, sticky r"\$\{(?:\\\n)*(?:[ \t\n]|\|)");
regex!(
    ZSH_SAFE_FLAGS_RE,
    sticky r"\$\{\((?:[@AaBbCcDEfFikLMmNnOopqStUuVvWwXZz0-]|q[-+]?|[js]([^({\[<\\])(?:(?!\1)[^)])*\1)*\)"
);
regex!(ZSH_GLOB_MODIFIER_RE, sticky r"(?:[\^=~#+]|\\\n)*~");
regex!(ZSH_SUBSCRIPT_RE, sticky r"\$(?:[\^=+~#]|\\\n)*(?:[A-Za-z_](?:[A-Za-z0-9_]|\\\n)*|[@*#?$!0-9-])(?:\\\n)*\[");
regex!(LINE_CONTINUATION_RE, sticky r"(?:\\\n)*");
regex!(PARAMETER_PREFIX_RE, sticky r"(?:\\\n)*(?:\([^)]*\))?(?:[!#^=+~]|\\\n)*");
regex!(PARAMETER_NAME_RE, sticky r"(?:[A-Za-z_](?:[A-Za-z0-9_]|\\\n)*|\d(?:\d|\\\n)*|[@*#?$!-])(?:\\\n)*");
regex!(PARAMETER_PROMPT_RE, sticky r"@(?:\\\n)*[EP](?:\\\n)*\}");

// Word tests (non-sticky)
regex!(RE_ZSH_SINK_WORD, r"^(?:-[A-Za-z]*v)?(?:(?:dis_)?(?:functions|[gs]?aliases)|options|commands)(?:\+?=|\[|$)");
regex!(RE_SET_A, r"^[-+][A-Za-z0-9]*A");
regex!(RE_EFIN, r"[EFin]");
regex!(RE_DECL_ARRAY_VALUE, r"\+?=(?:\(|[$`])");
regex!(RE_PRINT_V, r"^-[A-Za-z]*v");
regex!(RE_PRINTF_V, r"^-v");
regex!(RE_WAIT_P, r"^-[nf]*p");
regex!(RE_WAIT_P_END, r"^-[nf]*p$");
regex!(RE_EXEC_A, r"^-[cl]*a$");
regex!(RE_SET_K, r"^-[A-Za-z0-9]*k");
regex!(RE_SET_O, r"^[-+][A-Za-z0-9]*o");
regex!(RE_SETOPT_M, r"^-[A-Za-z0-9]*m");
regex!(RE_SETOPT_STRIP, r"(?i)^[-+][A-Za-z0-9]*?o");
regex!(RE_SETOPT_NAMES, r"^(?:no)?(?:globsubst|kshglob|promptsubst|promptvars|aliases)$");
regex!(RE_CF, r"[CF]");
regex!(RE_NAME, r"^[A-Za-z_][A-Za-z0-9_]*$");
regex!(RE_NAME_PLUS, r"^[A-Za-z_][A-Za-z0-9_]*\+?$");
regex!(RE_DOLLAR_BACKTICK, r"[$`]");
regex!(RE_PATTERN_TAIL, r"[?*+@!]$");
regex!(RE_FD_PREFIX, r"^(?:\d+|\{[A-Za-z_][A-Za-z0-9_]*\})$");
regex!(RE_ARITH_SINK, r"[A-Za-z_]|(?:\$(?![?#!-]|\$(?!\[)))");
regex!(RE_OPEN_CHARS, r"[({`]");
regex!(RE_DOLLAR_QUOTE_POSIX, r#"[#"\n]|\\'"#);
regex!(RE_ARITH_QUOTE_REJECT, r#"[()\[\];"}]"#);
regex!(RE_NULLCMD, r"^(?:READ)?NULLCMD$");
regex!(RE_ZSH_SINK_NAME, r"^(?:(?:dis_)?(?:functions|[gs]?aliases)|options|commands)$");
regex!(RE_BANG_PARAM, r"^(?:\\\n)*!");

// ---------------------------------------------------------------------------------------------
// Bash scanner state

/// Shared across one scan: the work budget, the shell dialect, and whether a bound expansion reaches a variable sink.
struct BashContext {
    remaining: i64,
    dialect: Dialect,
    bound_dollar: bool,
    bound_open: bool,
    variable_sink: bool,
}

#[derive(Clone)]
struct BashHeredoc {
    delimiter: String,
    dash_delimiter: Option<String>,
    quoted: bool,
    tabs: bool,
    binding: Option<bool>,
    command: Option<CmdRef>,
    start: Option<usize>,
    end: usize,
}

type Heredocs = Rc<RefCell<Vec<BashHeredoc>>>;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Kind {
    If,
    While,
    Until,
    For,
    Case,
    Repeat,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Phase {
    Header,
    Condition,
    Pattern,
    Body,
    Do,
}

struct Structure {
    kind: Kind,
    phase: Phase,
    count: usize,
    saw_else: bool,
    saw_in: bool,
    pattern_started: bool,
    parenthesized: bool,
}

/// A word's cooked text, the literal text it decodes to apart from its expansions, and the literal text of its
/// command substitutions' words, which stands in for their output.
#[derive(Default, Clone)]
struct BashText {
    word: String,
    literal: String,
    output: String,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Evaluation {
    Arithmetic,
    Binding,
    Deferred,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Close {
    Paren,
    Brace,
    Nofork,
}

struct BashState<'o> {
    commands: Vec<CmdRef>,
    nested_commands: Vec<CmdRef>,
    words: Vec<String>,
    raw_words: Vec<String>,
    heredocs: Heredocs,
    structures: Vec<Structure>,
    text: BashText,
    word_started: bool,
    word_start: usize,
    word_end: usize,
    command_start: Option<usize>,
    command_end: usize,
    resource_end: Option<usize>,
    redirect_word_count: Option<usize>,
    ampersand_redirect_words: Option<usize>,
    command_word_index: isize,
    assignment_word: bool,
    assignment_head_unsafe: bool,
    invalid: Option<OpaqueReason>,
    redirect_target: bool,
    here_string: bool,
    has_redirect: bool,
    compound_end: bool,
    dangling: bool,
    outer: Option<&'o mut BashText>,
}

impl BashState<'_> {
    fn structure(&mut self) -> Option<&mut Structure> {
        self.structures.last_mut()
    }
    fn structure_ref(&self) -> Option<&Structure> {
        self.structures.last()
    }
    fn word_at(&self, i: isize) -> Option<&str> {
        if i < 0 { None } else { self.words.get(i as usize).map(String::as_str) }
    }
    fn set_invalid(&mut self, reason: OpaqueReason) {
        if self.invalid.is_none() {
            self.invalid = Some(reason);
        }
    }
}

pub fn scan(input: &str, dialect: Dialect) -> ScanOutcome {
    if utf16_len(input) > MAX_INPUT_LENGTH {
        return ScanOutcome::Opaque(O::InvalidStructure);
    }
    let input = Input::new(input);
    let mut context = BashContext {
        remaining: (MAX_INPUT_LENGTH * MAX_SUBSTITUTION_DEPTH) as i64,
        dialect,
        bound_dollar: false,
        bound_open: false,
        variable_sink: false,
    };
    match scan_bash(&input, 0, 0, &mut context, None, None, None) {
        Err(reason) => ScanOutcome::Opaque(reason),
        Ok(_) if context.bound_dollar && context.bound_open && context.variable_sink => {
            ScanOutcome::Opaque(O::DynamicExecution)
        }
        Ok((commands, _)) => ScanOutcome::Scanned(unwrap_commands(commands)),
    }
}

fn unwrap_commands(commands: Vec<CmdRef>) -> Vec<Command> {
    commands.into_iter().map(|c| c.borrow().clone()).collect()
}

fn bash_statement(state: &mut BashState) {
    if let Some(structure) = state.structure() {
        structure.count += 1;
    }
}

fn bash_in_header(state: &BashState) -> bool {
    matches!(state.structure_ref().map(|s| s.phase), Some(Phase::Header | Phase::Pattern | Phase::Do))
}

fn at_command_start(state: &BashState) -> bool {
    state.words.is_empty() && !state.has_redirect && !state.compound_end && !bash_in_header(state)
}

fn pending(state: &BashState) -> bool {
    state.word_started || !state.words.is_empty() || state.has_redirect || state.compound_end
}

/// Ends the current list; false when an operator still awaits its command.
fn end_bash_list(input: &Input, state: &mut BashState, ctx: &mut BashContext) -> bool {
    if pending(state) {
        finish_bash_command(input, state, ctx, false);
    }
    !state.dangling
}

fn close_bash_list(input: &Input, mut state: BashState, ctx: &mut BashContext, end: usize) -> R<(Vec<CmdRef>, usize)> {
    if !end_bash_list(input, &mut state, ctx) {
        return Err(O::InvalidStructure);
    }
    if let Some(invalid) = state.invalid {
        return Err(invalid);
    }
    if !state.structures.is_empty() {
        return Err(O::CompoundCommand);
    }
    Ok((state.commands, end))
}

fn finish_bash_word(input: &Input, state: &mut BashState, ctx: &mut BashContext) {
    if !state.word_started {
        return;
    }
    if state.text.word.contains("PS4") {
        ctx.variable_sink = true;
    }
    let evaluation = bash_word_evaluation(state, ctx);
    if ctx.dialect != Dialect::Bash
        && (state.text.word.contains("NULLCMD")
            || ((state.assignment_word || evaluation == Some(Evaluation::Binding))
                && test(&RE_ZSH_SINK_WORD, &state.text.word)))
    {
        state.set_invalid(O::DynamicExecution);
    }
    if let Some(evaluation) = evaluation {
        if bash_evaluates_expansion(&state.text, evaluation, ctx) {
            state.set_invalid(O::DynamicExecution);
        }
    }
    if let Some(outer) = state.outer.as_deref_mut() {
        outer.output.push_str(&format!(" {} {}", state.text.literal, state.text.output));
    }
    let redirect_target = state.redirect_target;
    let repeat_header = matches!(state.structure_ref(), Some(s) if s.kind == Kind::Repeat && s.phase == Phase::Header);
    if !redirect_target && repeat_header {
        state.structure().unwrap().phase = Phase::Do;
    }
    if !redirect_target && !repeat_header {
        if !state.assignment_word && state.command_word_index < 0 {
            state.command_word_index = state.words.len() as isize;
        }
        if state.command_start.is_none() {
            state.command_start = Some(state.word_start);
        }
        let word = std::mem::take(&mut state.text.word);
        state.words.push(word);
        // Unquoted trailing continuations are ignored syntax, not part of the raw token.
        state.raw_words.push(input.slice(state.word_start, state.word_end).to_string());
        state.command_end = state.word_end;
    }
    state.redirect_target = false;
    state.here_string = false;
    state.text.word.clear();
    state.text.literal.clear();
    state.text.output.clear();
    state.word_started = false;
    state.assignment_word = false;
    state.assignment_head_unsafe = false;
}

/// How the shell evaluates the current word's literal text.
fn bash_word_evaluation(state: &BashState, ctx: &BashContext) -> Option<Evaluation> {
    if state.redirect_target {
        return if state.here_string { Some(Evaluation::Deferred) } else { None };
    }
    if let Some(structure) = state.structure_ref() {
        if structure.kind == Kind::Repeat && structure.phase == Phase::Header {
            return Some(Evaluation::Arithmetic);
        }
        if structure.kind == Kind::For && structure.phase == Phase::Header && structure.saw_in {
            return Some(Evaluation::Deferred);
        }
    }
    if state.command_word_index < 0 {
        return if state.assignment_word { Some(Evaluation::Deferred) } else { None };
    }
    let name = bash_builtin_index(state, ctx);
    let command = state.word_at(name)?;
    let rest = |from: isize| -> &[String] {
        let from = from.max(0) as usize;
        if from >= state.words.len() { &[] } else { &state.words[from..] }
    };
    if command == "let" || (ctx.dialect != Dialect::Bash && has(ZSH_ARITHMETIC_BUILTINS, command)) {
        return Some(Evaluation::Arithmetic);
    }
    if command == "set" {
        return if rest(name + 1).iter().any(|w| test(&RE_SET_A, w)) {
            Some(Evaluation::Binding)
        } else {
            Some(Evaluation::Deferred)
        };
    }
    if has(BASH_BINDING_BUILTINS, command) {
        let options = rest(name + 1);
        if options.iter().any(|w| w.starts_with('-') && test(&RE_EFIN, w)) {
            return Some(Evaluation::Arithmetic);
        }
        // A plain `name=value` declaration without options binds a scalar unless Bash re-parses an existing array.
        if state.assignment_word
            && has(BASH_ASSIGNMENT_DECLARATIONS, command)
            && options.iter().all(|w| !w.starts_with('-'))
            && (command == "export"
                || command == "readonly"
                || ctx.dialect == Dialect::Zsh
                || !test(&RE_DECL_ARRAY_VALUE, &state.text.word))
        {
            return Some(Evaluation::Deferred);
        }
        return Some(Evaluation::Binding);
    }
    if command == "printf" || command == "print" {
        let flag: &Regex = if command == "print" { &RE_PRINT_V } else { &RE_PRINTF_V };
        let any = rest(name + 1).iter().any(|w| test(flag, w)) || test(flag, &state.text.word);
        return if any { Some(Evaluation::Binding) } else { None };
    }
    if command == "wait" {
        return if test(&RE_WAIT_P, &state.text.word)
            || test(&RE_WAIT_P_END, state.words.last().map(String::as_str).unwrap_or(""))
        {
            Some(Evaluation::Binding)
        } else {
            None
        };
    }
    if command == "test" || command == "[" {
        let last = state.words.last().map(String::as_str);
        if last == Some("-v") {
            return Some(Evaluation::Binding);
        }
        if ctx.dialect != Dialect::Bash && last == Some("-t") {
            return Some(Evaluation::Arithmetic);
        }
    }
    None
}

/// Index of the word naming the builtin that runs, skipping precommand wrappers and their options.
fn bash_builtin_index(state: &BashState, ctx: &BashContext) -> isize {
    let mut name = state.command_word_index;
    loop {
        let current = state.word_at(name).unwrap_or("");
        if !(has(BASH_PRECOMMANDS, current) || (ctx.dialect != Dialect::Bash && has(ZSH_PRECOMMANDS, current))) {
            break;
        }
        let precommand = current.to_string();
        name += 1;
        while precommand != "-" && state.word_at(name).is_some_and(|w| w.starts_with('-')) {
            if precommand == "exec" && test(&RE_EXEC_A, state.word_at(name).unwrap()) {
                name += 1;
            }
            name += 1;
        }
    }
    name
}

fn finish_bash_command(input: &Input, state: &mut BashState, ctx: &mut BashContext, boundary: bool) {
    finish_bash_word(input, state, ctx);
    if state.redirect_target {
        state.set_invalid(O::InvalidRedirect);
    }
    state.redirect_target = false;
    if state.compound_end && !state.words.is_empty() {
        state.set_invalid(O::InvalidStructure);
    }
    let name = state.command_word_index;
    let in_header = bash_in_header(state);
    if name >= 0 && state.word_at(name).is_none_or(str::is_empty) {
        state.set_invalid(O::InvalidStructure);
    }
    if name >= 0 && !in_header {
        let n = name as usize;
        let start = state.command_start.unwrap_or(0);
        let end = state.resource_end.unwrap_or(state.word_end);
        let resource = input.slice(start, end).trim_matches([' ', '\t', '\n']).to_string();
        let first_raw = state.raw_words.first().map(String::as_str).unwrap_or("");
        let declaration = n == 0 && has(BASH_DECLARATIONS, first_raw) && resource.starts_with(first_raw);
        let redirect_word_count = match state.redirect_word_count {
            Some(count) if count < state.words.len() => Some(count.saturating_sub(n)),
            _ => None,
        };
        let command = Rc::new(RefCell::new(Command {
            resource,
            words: state.words.get(n..).map(<[String]>::to_vec).unwrap_or_default(),
            raw_words: state.raw_words.get(n..).map(<[String]>::to_vec).unwrap_or_default(),
            word_ends: None,
            statement_head: false,
            declaration,
            redirect_word_count,
        }));
        state.commands.push(command.clone());
        let builtin = bash_builtin_index(state, ctx);
        let builtin_name = state.word_at(builtin).map(str::to_string);
        let builtin_args: Vec<String> = if builtin + 1 < state.words.len() as isize {
            state.words[(builtin + 1) as usize..].to_vec()
        } else {
            vec![]
        };
        let builtin_name_str = builtin_name.as_deref();
        if builtin_name_str == Some("eval") {
            ctx.variable_sink = true;
        }
        if builtin_name_str == Some("set") && builtin_args.iter().any(|w| test(&RE_SET_K, w) || w == "keyword") {
            state.set_invalid(O::DynamicExecution);
        }
        // Dash expands an alias defined earlier in the same script, which can name any command.
        if builtin_name_str == Some("alias") && builtin_args.iter().any(|w| w.contains('=')) {
            state.set_invalid(O::DynamicExecution);
        }
        // Zsh options such as globsubst, kshglob, and promptsubst enable evaluation on expansions and print -P.
        if ctx.dialect != Dialect::Bash
            && (builtin_name_str == Some("emulate")
                || ((builtin_name_str == Some("setopt")
                    || builtin_name_str == Some("unsetopt")
                    || (builtin_name_str == Some("set") && builtin_args.iter().any(|w| test(&RE_SET_O, w))))
                    && builtin_args.iter().any(|w| {
                        if test(&RE_SETOPT_M, w) {
                            return true;
                        }
                        let stripped = RE_SETOPT_STRIP.replace(w, "").to_lowercase().replace(['_', '-'], "");
                        test(&RE_SETOPT_NAMES, &stripped)
                    })))
        {
            state.set_invalid(O::DynamicExecution);
        }
        // Bash compgen -C and -F execute their argument immediately to generate completions.
        if ctx.dialect != Dialect::Zsh
            && builtin_name_str == Some("compgen")
            && builtin_args.iter().any(|w| w.starts_with('-') && test(&RE_CF, w))
        {
            state.set_invalid(O::DynamicExecution);
        }
        if has(BASH_BINDING_BUILTINS, builtin_name_str.unwrap_or("")) {
            for heredoc in state.heredocs.borrow_mut().iter_mut() {
                if heredoc.binding.is_none() {
                    heredoc.binding = Some(true);
                }
            }
        }
        if state.resource_end.is_none() {
            for heredoc in state.heredocs.borrow_mut().iter_mut() {
                if heredoc.command.is_some() {
                    continue;
                }
                heredoc.command = Some(command.clone());
                heredoc.start = state.command_start;
            }
        }
    }
    if boundary && state.words.is_empty() && !state.has_redirect && !state.compound_end && !in_header {
        state.set_invalid(O::InvalidStructure);
    }
    if state.words.len() > state.ampersand_redirect_words.unwrap_or(usize::MAX) {
        state.set_invalid(O::InvalidRedirect);
    }
    let nested = std::mem::take(&mut state.nested_commands);
    state.commands.extend(nested);
    if !in_header && (!state.words.is_empty() || state.has_redirect) {
        bash_statement(state);
    }
    state.words.clear();
    state.raw_words.clear();
    state.command_word_index = -1;
    state.command_start = None;
    state.has_redirect = false;
    state.resource_end = None;
    state.redirect_word_count = None;
    state.ampersand_redirect_words = None;
    state.compound_end = false;
    state.dangling = false;
}

fn new_state<'o>(start: usize, heredocs: Option<Heredocs>, outer: Option<&'o mut BashText>) -> BashState<'o> {
    BashState {
        commands: vec![],
        nested_commands: vec![],
        words: vec![],
        raw_words: vec![],
        heredocs: heredocs.unwrap_or_default(),
        structures: vec![],
        text: BashText::default(),
        word_started: false,
        word_start: start,
        word_end: start,
        command_start: None,
        command_end: start,
        resource_end: None,
        redirect_word_count: None,
        ampersand_redirect_words: None,
        command_word_index: -1,
        assignment_word: false,
        assignment_head_unsafe: false,
        invalid: None,
        redirect_target: false,
        here_string: false,
        has_redirect: false,
        compound_end: false,
        dangling: false,
        outer,
    }
}

fn scan_bash(
    input: &Input,
    start: usize,
    depth: usize,
    ctx: &mut BashContext,
    close: Option<Close>,
    // A group shares its parent's heredocs; a substitution starts its own.
    heredocs: Option<Heredocs>,
    outer: Option<&mut BashText>,
) -> R<(Vec<CmdRef>, usize)> {
    if depth > MAX_SUBSTITUTION_DEPTH || ctx.remaining < 0 {
        return Err(O::InvalidStructure);
    }
    let shared = heredocs.is_some();
    let mut state = new_state(start, heredocs, outer);
    let len = input.len();
    let mut index = start as isize - 1;
    loop {
        index += 1;
        if index as usize >= len {
            break;
        }
        let i = index as usize;
        ctx.remaining -= 1;
        if ctx.remaining < 0 {
            return Err(O::InvalidStructure);
        }
        let c = input.chars[i];
        // Line continuations are removed before tokens are read.
        if c == '\\' && input.is(i + 1, '\n') {
            index += 1;
            continue;
        }
        if !state.word_started {
            state.word_start = i;
            if c == ' ' || c == '\t' {
                continue;
            }
            if c == '}'
                && state.words.is_empty()
                && !state.redirect_target
                && matches!(close, Some(Close::Brace | Close::Nofork))
                && state.structures.is_empty()
                && (shared || state.heredocs.borrow().is_empty())
                && (close == Some(Close::Nofork) || input.sticky(&BRACE_CLOSE_AHEAD_RE, i + 1).is_some())
            {
                return close_bash_list(input, state, ctx, i);
            }
            if c == '}' {
                return Err(if state.structures.is_empty() { O::InvalidStructure } else { O::CompoundCommand });
            }
            if let Some(step) = scan_bash_command_start(input, &mut state, i, depth, ctx)? {
                index = step;
                continue;
            }
        }
        if c == '\\' && i + 1 >= len {
            return Err(O::UnterminatedEscape);
        }
        if !state.assignment_word
            && ("'\"\\".contains(c) || (c == '$' && (input.is(i + 1, '\'') || input.is(i + 1, '"'))))
        {
            state.assignment_head_unsafe = true;
        }
        let in_case_pattern = state.structure_ref().map(|s| s.phase) == Some(Phase::Pattern);
        // Elsewhere a parenthesized group inside a word is a Zsh glob qualifier, which can run code.
        let array = (c == '=' || c == '(')
            && state.assignment_word
            && state.text.word.len() > 1
            && state.text.word.ends_with('=')
            && !state.text.word.ends_with("==")
            && (state.command_word_index < 0
                || state
                    .raw_words
                    .get(state.command_word_index as usize)
                    .is_some_and(|w| has(BASH_ASSIGNMENT_DECLARATIONS, w)));
        if c == '=' && (!state.word_started || array) && input.is(i + 1, '(') {
            let end = scan_bash_nested(input, i + 2, depth, ctx, &mut state.nested_commands, Close::Paren, None)?;
            state.word_started = true;
            state.text.word.push_str(input.slice(i, end + 1));
            index = end as isize;
            state.word_end = end + 1;
            continue;
        }
        let unit = if BASH_UNIT_STARTS.contains(c) {
            let allow = !state.assignment_word
                && state.command_word_index >= 0
                && !state.raw_words.first().is_some_and(|w| has(BASH_DECLARATIONS, w));
            scan_bash_unit(input, i, depth, ctx, &mut state.nested_commands, Mode::Word, allow, Some(&mut state.text))?
        } else {
            None
        };
        if let Some(unit) = unit {
            state.word_started = true;
            index = unit as isize;
            state.word_end = unit + 1;
            continue;
        }
        if c == '['
            && !in_case_pattern
            && !state.assignment_word
            && !state.assignment_head_unsafe
            && state.command_word_index < 0
            && test(&RE_NAME, &state.text.word)
        {
            let mut subscript: Vec<CmdRef> = vec![];
            let end = scan_bash_subscript(input, i + 1, depth, ctx, &mut subscript, &SPAN_ASSIGNMENT_SUBSCRIPT)?;
            let assign = bash_operator(input, end + 1, &["+=", "="])?;
            if assign.is_some() {
                // Dash has no arrays and word-splits unquoted expansions inside `name[subscript]=value`.
                if ctx.dialect == Dialect::Posix && test(&RE_DOLLAR_BACKTICK, input.slice(i + 1, end)) {
                    return Err(O::InvalidStructure);
                }
                state.nested_commands.extend(subscript);
                state.assignment_word = true;
                state.text.word.push_str(input.slice(i, end + 1));
                index = end as isize;
                state.word_end = end + 1;
                continue;
            }
        }
        if c == '('
            && (array
                || ((in_case_pattern || ctx.dialect == Dialect::Bash) && test(&RE_PATTERN_TAIL, &state.text.word)))
        {
            let mode = if array { ListMode::Array } else { ListMode::Pattern };
            let end = scan_bash_array_or_pattern(input, i, depth + 1, ctx, &mut state.nested_commands, mode)?;
            if bash_crosses_heredoc(&state, input, i, end) {
                return Err(O::Heredoc);
            }
            // Zsh ends an array assignment at its closing parenthesis, so text right after it starts a command.
            if array && end + 1 < len && !" \t\n;&|<>)".contains(input.chars[end + 1]) {
                return Err(O::InvalidStructure);
            }
            state.word_started = true;
            state.text.word.push_str(input.slice(i, end + 1));
            index = end as isize;
            state.word_end = end + 1;
            continue;
        }
        if c == '#' && !state.word_started {
            match input.index_of_char('\n', i) {
                None => break,
                Some(newline) => {
                    index = newline as isize - 1;
                    continue;
                }
            }
        }
        if "<>&|;\n".contains(c) {
            index = scan_bash_operator_or_separator(input, &mut state, i, depth, ctx)?;
            continue;
        }
        if in_case_pattern && c == ')' {
            finish_bash_word(input, &mut state, ctx);
            index -= 1;
            continue;
        }
        if c == ')' {
            if close != Some(Close::Paren)
                || !state.structures.is_empty()
                || (!shared && !state.heredocs.borrow().is_empty())
            {
                return Err(O::CompoundCommand);
            }
            return close_bash_list(input, state, ctx, i);
        }
        if c == '(' {
            return Err(if state.word_started { O::DynamicExecution } else { O::CompoundCommand });
        }
        if js_space(c) && !" \t\n".contains(c) {
            return Err(O::InvalidStructure);
        }
        if c == ' ' || c == '\t' {
            finish_bash_word(input, &mut state, ctx);
            continue;
        }
        state.word_started = true;
        if c == '='
            && !state.assignment_word
            && !state.assignment_head_unsafe
            && test(&RE_NAME_PLUS, &state.text.word)
        {
            state.assignment_word = true;
        }
        bash_append_char(&mut state.text, c);
        state.word_end = i + 1;
    }

    if let Some(close) = close {
        return Err(if close == Close::Paren { O::CommandSubstitution } else { O::InvalidStructure });
    }
    if !state.heredocs.borrow().is_empty() {
        return Err(O::Heredoc);
    }
    close_bash_list(input, state, ctx, len)
}

// Steps return the last consumed index.
fn scan_bash_operator_or_separator(
    input: &Input,
    state: &mut BashState,
    index: usize,
    depth: usize,
    ctx: &mut BashContext,
) -> R<isize> {
    let c = input.chars[index];
    let redirect = if "<>&".contains(c) { bash_operator(input, index, BASH_REDIRECTS)? } else { None };
    let case_head = state.structure_ref().map(|s| (s.kind, s.phase));
    if redirect.is_some() && matches!(case_head, Some((Kind::Case, phase)) if phase != Phase::Body) {
        return Err(O::InvalidRedirect);
    }
    if let Some(redirect) = redirect {
        state.has_redirect = true;
        if state.command_start.is_none() {
            state.command_start = Some(state.word_start);
        }
        if state.redirect_target {
            state.set_invalid(O::InvalidRedirect);
        }
        let fd_prefix = state.word_started && !state.assignment_head_unsafe && test(&RE_FD_PREFIX, &state.text.word);
        if fd_prefix {
            // A continuation separates the legacy number token from the redirect descriptor.
            if state.word_end < index {
                state.command_end = state.word_end;
            }
            state.text.word.clear();
            state.text.literal.clear();
            state.text.output.clear();
            state.word_started = false;
        }
        if !fd_prefix {
            finish_bash_word(input, state, ctx);
        }
        if redirect.starts_with('&') && ctx.dialect == Dialect::Posix && state.ampersand_redirect_words.is_none() {
            state.ampersand_redirect_words = Some(state.words.len());
        }
        // Trailing redirects wrap a whole list/pipeline in the legacy grammar, not its last command.
        if state.redirect_word_count.is_none() && state.command_word_index >= 0 {
            state.redirect_word_count = Some(state.words.len());
            if state.dangling {
                state.resource_end = Some(state.command_end);
            }
        }
        if redirect == "<<" || redirect == "<<-" {
            let Some(delimiter) = bash_heredoc_delimiter(input, index + redirect.len(), redirect == "<<-") else {
                return Err(O::InvalidRedirect);
            };
            let end = delimiter.end;
            state.heredocs.borrow_mut().push(delimiter);
            state.word_end = end + 1;
            state.redirect_target = false;
            return Ok(end as isize);
        }
        state.redirect_target = true;
        state.here_string = redirect == "<<<";
        return Ok((index + redirect.len()) as isize - 1);
    }
    if state.structure_ref().map(|s| s.phase) == Some(Phase::Pattern) && c == '|' {
        finish_bash_word(input, state, ctx);
        return Ok(index as isize - 1);
    }
    let structure = state.structure_ref().map(|s| (s.kind, s.phase, s.saw_in));
    let case_body = matches!(structure, Some((Kind::Case, Phase::Body, _)));
    let operator = bash_operator(input, index, if case_body { BASH_CASE_OPERATORS } else { BASH_LIST_OPERATORS })?;
    let separator: String = operator.map(str::to_string).unwrap_or_else(|| c.to_string());
    if case_body && separator.starts_with(';') && separator.len() > 1 {
        if !end_bash_list(input, state, ctx) {
            return Err(O::InvalidStructure);
        }
        let s = state.structure().unwrap();
        s.phase = Phase::Pattern;
        s.pattern_started = false;
        return Ok((index + separator.len()) as isize - 1);
    }
    if let Some((Kind::Case, Phase::Header | Phase::Pattern, _)) = structure {
        if separator != "\n" {
            return Err(O::CompoundCommand);
        }
        if structure.unwrap().1 == Phase::Pattern && (state.word_started || !state.words.is_empty()) {
            return Err(O::CompoundCommand);
        }
        finish_bash_word(input, state, ctx);
        return read_bash_heredocs(input, state, index, depth, ctx);
    }
    if let Some((Kind::For | Kind::Repeat, phase @ (Phase::Header | Phase::Do), saw_in)) = structure {
        if separator != ";" && separator != "\n" {
            return Err(O::CompoundCommand);
        }
        if phase == Phase::Header && !saw_in && !state.word_started && state.words.is_empty() {
            return Err(O::CompoundCommand);
        }
    }
    if separator == "\n" && !pending(state) {
        return read_bash_heredocs(input, state, index, depth, ctx);
    }
    finish_bash_command(input, state, ctx, true);
    if let Some((Kind::For, Phase::Header, _)) = structure {
        state.structure().unwrap().phase = Phase::Do;
    }
    state.dangling = separator != "&" && separator != ";" && separator != "\n";
    // Reprocess the newline to read pending heredoc bodies.
    if separator == "\n" && !state.heredocs.borrow().is_empty() {
        return Ok(index as isize - 1);
    }
    Ok((index + separator.len()) as isize - 1)
}

fn bash_command_key(command: &CmdRef) -> (String, Vec<String>) {
    let command = command.borrow();
    (command.resource.clone(), command.words.clone())
}

// Reads the pending heredoc bodies after the newline at index and returns the last index they cover.
fn read_bash_heredocs(
    input: &Input,
    state: &mut BashState,
    index: usize,
    depth: usize,
    ctx: &mut BashContext,
) -> R<isize> {
    let mut end = index;
    let heredocs: Vec<BashHeredoc> = state.heredocs.borrow_mut().drain(..).collect();
    for heredoc in heredocs {
        let Some((source, body_end)) = bash_heredoc(input, end + 1, &heredoc) else {
            return Err(O::Heredoc);
        };
        if let Some(command) = &heredoc.command {
            command.borrow_mut().resource = js_trim(input.slice(heredoc.start.unwrap_or(0), body_end)).to_string();
        }
        let mut text = if heredoc.quoted {
            BashText { word: source.clone(), literal: source.clone(), output: String::new() }
        } else {
            BashText::default()
        };
        if !heredoc.quoted {
            let body = Input::new(&source);
            scan_bash_span(&body, 0, depth, ctx, &mut state.commands, &SPAN_HEREDOC, true, Some(&mut text))?;
        }
        if heredoc.binding == Some(true) {
            bash_evaluates_expansion(&text, Evaluation::Deferred, ctx);
        }
        end = body_end;
    }
    Ok(end as isize)
}

// Whether a construct read as one unit holds a newline token that would start pending heredoc bodies.
fn bash_crosses_heredoc(state: &BashState, input: &Input, start: usize, end: usize) -> bool {
    !state.heredocs.borrow().is_empty() && input.slice(start, end).contains('\n')
}

fn token_at(input: &Input, index: usize) -> Option<&str> {
    input.sticky(&TOKEN_RE, index).map(|n| input.slice(index, index + n))
}

fn scan_bash_command_start(
    input: &Input,
    state: &mut BashState,
    index: usize,
    depth: usize,
    ctx: &mut BashContext,
) -> R<Option<isize>> {
    let c = input.chars[index];
    let snapshot = state.structure_ref().map(|s| (s.kind, s.phase, s.pattern_started));
    if let Some((_, Phase::Pattern, pattern_started)) = snapshot {
        if !pattern_started && state.words.is_empty() && c == '(' {
            state.structure().unwrap().pattern_started = true;
            return Ok(Some(index as isize));
        }
        if c == '|' {
            if state.words.is_empty() {
                return Err(O::CompoundCommand);
            }
            finish_bash_word(input, state, ctx);
            state.structure().unwrap().pattern_started = true;
            return Ok(Some(index as isize));
        }
        if c == ')' {
            if state.words.is_empty() {
                return Err(O::CompoundCommand);
            }
            finish_bash_command(input, state, ctx, false);
            let s = state.structure().unwrap();
            s.phase = Phase::Body;
            s.count = 0;
            return Ok(Some(index as isize));
        }
    }
    if let Some((Kind::For, Phase::Header, _)) = snapshot {
        if c == '(' && !input.is(index + 1, '(') {
            let end =
                scan_bash_array_or_pattern(input, index, depth + 1, ctx, &mut state.nested_commands, ListMode::Array)?;
            if bash_crosses_heredoc(state, input, index, end) {
                return Err(O::Heredoc);
            }
            finish_bash_command(input, state, ctx, false);
            // Zsh permits a sublist or brace group directly after the value list, without do/done.
            let s = state.structure().unwrap();
            s.phase = Phase::Do;
            s.parenthesized = true;
            if input.sticky(&DO_AHEAD_RE, end + 1).is_none() {
                state.structures.pop();
            }
            return Ok(Some(end as isize));
        }
    }
    // `structure` in upstream is captured before this pop and stays the popped value afterwards.
    let mut stale: Option<(Kind, Phase)> = None;
    if let Some((Kind::Repeat, Phase::Do, _)) = snapshot {
        if c != '\n' && c != '#' && c != ';' && token_at(input, index) != Some("do") {
            state.structures.pop();
            stale = Some((Kind::Repeat, Phase::Do));
        }
    }
    if let Some(step) = scan_bash_keyword(input, state, index, depth, ctx)? {
        return Ok(Some(step));
    }
    let current = || -> Option<(Kind, Phase)> { stale.or_else(|| state.structure_ref().map(|s| (s.kind, s.phase))) };
    let structure = current();
    if let Some((Kind::For, Phase::Do)) = structure {
        if c != '\n' && c != '#' && c != ';' {
            if c != '{' || input.sticky(&SPACE_CONTINUATION_AHEAD_RE, index + 1).is_none() {
                return Err(O::CompoundCommand);
            }
            state.structures.pop();
        }
    }
    let at_start = at_command_start(state);
    let function_head = if at_start && c == '(' { bash_function_head_length(input, index) } else { 0 };
    if function_head > 0 {
        return Ok(Some((index + function_head) as isize - 1));
    }
    if at_start && c == '!' && input.sticky(&NEGATION_AHEAD_RE, index + 1).is_some() {
        return Ok(Some(index as isize));
    }
    let for_header = structure == Some((Kind::For, Phase::Header));
    let compound_open =
        if (at_start || for_header) && (c == '(' || c == '[') { bash_operator(input, index, &["((", "[["])? } else { None };
    if compound_open == Some("((") {
        if for_header && !state.words.is_empty() {
            return Err(O::CompoundCommand);
        }
        let span = if for_header { &SPAN_FOR_ARITHMETIC } else { &SPAN_ARITHMETIC };
        match scan_bash_arithmetic(input, index + 2, depth, ctx, &mut state.commands, span)? {
            Some(end) => {
                if bash_crosses_heredoc(state, input, index, end) {
                    return Err(O::Heredoc);
                }
                if !for_header && ctx.dialect == Dialect::Posix {
                    // Dash reads `((` as two nested subshells, so posix reports both readings.
                    let (commands, sub_end) = scan_bash(input, index + 1, depth + 1, ctx, Some(Close::Paren), None, None)?;
                    if sub_end != end {
                        return Err(O::CompoundCommand);
                    }
                    let seen: HashSet<(String, Vec<String>)> = state.commands.iter().map(bash_command_key).collect();
                    state.commands.extend(commands.into_iter().filter(|c| !seen.contains(&bash_command_key(c))));
                }
                if for_header {
                    let s = state.structure().unwrap();
                    s.phase = Phase::Do;
                    s.saw_in = true;
                } else {
                    bash_statement(state);
                    state.compound_end = true;
                }
                return Ok(Some(end as isize));
            }
            None => {
                if for_header {
                    return Err(O::InvalidStructure);
                }
            }
        }
    }
    if at_start && (c == '(' || (c == '{' && input.sticky(&SPACE_CONTINUATION_AHEAD_RE, index + 1).is_some())) {
        let close = if c == '{' { Close::Brace } else { Close::Paren };
        let (commands, end) =
            scan_bash(input, index + 1, depth + 1, ctx, Some(close), Some(state.heredocs.clone()), None)?;
        if js_trim(input.slice(index + 1, end)).is_empty() {
            return Err(O::InvalidStructure);
        }
        state.commands.extend(commands);
        bash_statement(state);
        state.compound_end = true;
        return Ok(Some(end as isize));
    }
    if at_start && compound_open == Some("[[") && input.sticky(&SPACE_CONTINUATION_AHEAD_RE, index + 2).is_some() {
        let end = scan_bash_conditional(input, index + 2, depth, ctx, &mut state.commands)?;
        if bash_crosses_heredoc(state, input, index, end) {
            return Err(O::Heredoc);
        }
        bash_statement(state);
        state.compound_end = true;
        return Ok(Some(end as isize));
    }
    Ok(None)
}

fn scan_bash_keyword(
    input: &Input,
    state: &mut BashState,
    index: usize,
    depth: usize,
    ctx: &mut BashContext,
) -> R<Option<isize>> {
    let c = input.chars[index];
    if !(c.is_ascii_alphabetic() || c == '_') {
        return Ok(None);
    }
    let token: Option<String> = token_at(input, index).map(str::to_string);
    let token = token.as_deref();
    let end = (index + token.map_or(0, |t| t.chars().count())) as isize - 1;
    let snap = state.structure_ref().map(|s| (s.kind, s.phase, s.saw_in, s.parenthesized, s.pattern_started));
    if let Some((Kind::Case, Phase::Header, ..)) = snap {
        if token == Some("in") {
            if state.words.len() != 1 || state.has_redirect {
                return Err(O::CompoundCommand);
            }
            finish_bash_command(input, state, ctx, false);
            let s = state.structure().unwrap();
            s.phase = Phase::Pattern;
            s.pattern_started = false;
            return Ok(Some(end));
        }
    }
    if let Some((Kind::For, phase @ (Phase::Header | Phase::Do), false, false, _)) = snap {
        if token == Some("in") && (phase == Phase::Do || !state.words.is_empty()) {
            let s = state.structure().unwrap();
            s.saw_in = true;
            s.phase = Phase::Header;
            return Ok(Some(end));
        }
    }
    if let Some((_, Phase::Pattern, _, _, false)) = snap {
        if state.words.is_empty() && token == Some("esac") {
            state.structures.pop();
            bash_statement(state);
            state.compound_end = true;
            return Ok(Some(end));
        }
    }
    // POSIX `for name do` omits the in list.
    if let Some((Kind::For, Phase::Header, false, ..)) = snap {
        if state.words.len() == 1 && !state.has_redirect && token == Some("do") {
            finish_bash_command(input, state, ctx, false);
            state.structure().unwrap().phase = Phase::Do;
        }
    }
    let in_header = bash_in_header(state);
    if let Some(tok) = token {
        let phase_now = state.structure_ref().map(|s| s.phase);
        if ["then", "elif", "else", "fi", "do", "done", "esac"].contains(&tok)
            && state.words.is_empty()
            && !state.redirect_target
            && (!in_header || (tok == "do" && phase_now == Some(Phase::Do)))
        {
            // Bash and Dash read a reserved word after a redirect as a command word, while Zsh reads a keyword.
            if (state.has_redirect && ctx.dialect != Dialect::Zsh)
                || !end_bash_list(input, state, ctx)
                || state.structures.is_empty()
            {
                return Err(O::CompoundCommand);
            }
            let s = state.structure().unwrap();
            if tok == "then" {
                if s.kind != Kind::If || s.phase != Phase::Condition || s.count == 0 {
                    return Err(O::CompoundCommand);
                }
                s.phase = Phase::Body;
                s.count = 0;
                return Ok(Some(end));
            }
            if tok == "elif" || tok == "else" {
                if s.kind != Kind::If || s.phase != Phase::Body || s.count == 0 || s.saw_else {
                    return Err(O::CompoundCommand);
                }
                s.phase = if tok == "elif" { Phase::Condition } else { Phase::Body };
                s.saw_else = tok == "else";
                s.count = 0;
                return Ok(Some(end));
            }
            let looping = matches!(s.kind, Kind::For | Kind::While | Kind::Until | Kind::Repeat);
            if tok == "do" {
                if !looping
                    || !matches!(s.phase, Phase::Condition | Phase::Do)
                    || (s.kind != Kind::For && s.kind != Kind::Repeat && s.count == 0)
                {
                    return Err(O::CompoundCommand);
                }
                s.phase = Phase::Body;
                s.count = 0;
                return Ok(Some(end));
            }
            if (tok == "fi" && (s.kind != Kind::If || s.phase != Phase::Body || s.count == 0))
                || (tok == "done" && (!looping || s.phase != Phase::Body || s.count == 0))
                || (tok == "esac" && (s.kind != Kind::Case || s.phase == Phase::Header))
            {
                return Err(O::CompoundCommand);
            }
            state.structures.pop();
            bash_statement(state);
            state.compound_end = true;
            return Ok(Some(end));
        }
    }
    if !at_command_start(state) {
        return Ok(None);
    }
    let definition_length = bash_function_head_length(input, index);
    if definition_length > 0 {
        return Ok(Some((index + definition_length) as isize - 1));
    }
    let kind = match token {
        Some("if") => Some(Kind::If),
        Some("while") => Some(Kind::While),
        Some("until") => Some(Kind::Until),
        Some("for") | Some("select") => Some(Kind::For),
        Some("case") => Some(Kind::Case),
        Some("repeat") if ctx.dialect != Dialect::Bash => Some(Kind::Repeat),
        _ => None,
    };
    if let Some(kind) = kind {
        if depth + state.structures.len() >= MAX_SUBSTITUTION_DEPTH {
            return Err(O::CompoundCommand);
        }
        let phase = if matches!(kind, Kind::For | Kind::Case | Kind::Repeat) { Phase::Header } else { Phase::Condition };
        state.structures.push(Structure {
            kind,
            phase,
            count: 0,
            saw_else: false,
            saw_in: false,
            pattern_started: false,
            parenthesized: false,
        });
        // The compound command itself satisfies a preceding list operator.
        state.dangling = false;
        return Ok(Some(end));
    }
    if token == Some("coproc") {
        if let Some(n) = input.sticky(&COPROC_AHEAD_RE, index) {
            if n > 0 {
                return Ok(Some((index + n) as isize - 1));
            }
        }
    }
    if token == Some("time") {
        if let Some(n) = input.sticky(&TIME_AHEAD_RE, index) {
            if n > 0 {
                return Ok(Some((index + n) as isize - 1));
            }
        }
    }
    Ok(None)
}

fn bash_function_head_length(input: &Input, start: usize) -> usize {
    let Some(head) = input.sticky_caps(&FUNCTION_HEAD_RE, start) else {
        return 0;
    };
    let truthy = |i: usize| head.get(i).and_then(|g| g.as_deref()).is_some_and(|g| !g.is_empty());
    let group2 = head.get(2).and_then(|g| g.as_deref()).unwrap_or("");
    let reject = if truthy(1) { !truthy(2) } else { !truthy(3) || has(BASH_NON_FUNCTION_KEYWORDS, group2) };
    if reject {
        return 0;
    }
    let whole = head[0].as_deref().unwrap_or("").chars().count();
    let trailing = head.get(4).and_then(|g| g.as_deref()).unwrap_or("").chars().count();
    // Blanks, comments, and newlines before the body stay for the caller, where newlines start heredoc bodies.
    if input.sticky(&FUNCTION_BODY_RE, start + whole).is_some() { whole - trailing } else { 0 }
}

// Word text is unquoted. Quoted text follows double-quote rules. Arithmetic text, including subscripts,
// expands like double-quoted text while its matcher still lets a backslash escape any character.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Mode {
    Word,
    Quoted,
    Arithmetic,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum ListMode {
    Array,
    Pattern,
}

struct Span {
    open: Option<char>,
    close: Option<char>,
    reject: &'static str,
    mode: Mode,
}

static SPAN_DOUBLE: Span = Span { open: None, close: Some('"'), reject: "", mode: Mode::Quoted };
static SPAN_HEREDOC: Span = Span { open: None, close: None, reject: "", mode: Mode::Quoted };
// Bash rereads `((...))` it cannot parse as arithmetic, such as text with a comment, as nested subshells.
static SPAN_ARITHMETIC: Span = Span { open: Some('('), close: Some(')'), reject: ";#}", mode: Mode::Arithmetic };
static SPAN_FOR_ARITHMETIC: Span = Span { open: Some('('), close: Some(')'), reject: "#}", mode: Mode::Arithmetic };
static SPAN_BRACKET_ARITHMETIC: Span =
    Span { open: None, close: Some(']'), reject: ";|&<>()[\n'\"\\#", mode: Mode::Arithmetic };
static SPAN_SUBSCRIPT: Span = Span { open: Some('['), close: Some(']'), reject: "\"}", mode: Mode::Arithmetic };
static SPAN_QUOTED_SUBSCRIPT: Span = Span { open: Some('['), close: Some(']'), reject: "\"", mode: Mode::Quoted };
// Dash splits an assignment subscript at blanks and operators, so Bash and Zsh assignments diverge.
static SPAN_ASSIGNMENT_SUBSCRIPT: Span =
    Span { open: Some('['), close: Some(']'), reject: " \t\n\r\u{0B}\u{0C};&|<>()", mode: Mode::Arithmetic };
// Single quotes are literal here in some shells and quoting in others.
static SPAN_ARITHMETIC_QUOTE: Span = Span { open: None, close: Some('\''), reject: "()[];\"}", mode: Mode::Quoted };
static SPAN_ARITHMETIC_DOUBLE: Span = Span { open: None, close: Some('"'), reject: "()[];}", mode: Mode::Quoted };
static SPAN_PARAMETER_QUOTE: Span = Span { open: None, close: Some('\''), reject: "\"}[]", mode: Mode::Quoted };

fn bash_append(text: &mut BashText, value: &str) {
    text.word.push_str(value);
    text.literal.push_str(value);
}

fn bash_append_char(text: &mut BashText, value: char) {
    text.word.push(value);
    text.literal.push(value);
}

// Scans one quoting or expansion unit at index into commands, appending its text when given a sink.
// Returns the unit's last index, or None when the character is ordinary text.
#[allow(clippy::too_many_arguments)]
fn scan_bash_unit(
    input: &Input,
    index: usize,
    depth: usize,
    ctx: &mut BashContext,
    commands: &mut Vec<CmdRef>,
    mode: Mode,
    allow_bracket: bool,
    mut text: Option<&mut BashText>,
) -> R<Option<usize>> {
    let c = input.chars[index];
    let next = input.at(index + 1).unwrap_or('\0');
    if c == '\\' {
        if mode == Mode::Quoted && !"$`\"\\\n".contains(next) {
            return Ok(None);
        }
        if let Some(text) = text.as_deref_mut() {
            if next != '\n' {
                bash_append(text, input.slice(index + 1, index + 2));
            }
        }
        return Ok(Some(index + 1));
    }
    if c == '$' && ctx.dialect != Dialect::Bash && zsh_evaluates(input, index) {
        return Err(O::DynamicExecution);
    }
    if c == '$' && ctx.dialect != Dialect::Bash && (mode == Mode::Word || mode == Mode::Quoted) {
        if let Some(n) = input.sticky(&ZSH_SUBSCRIPT_RE, index) {
            let bracket = index + n;
            let mut subscript = BashText::default();
            let span = if mode == Mode::Quoted { &SPAN_QUOTED_SUBSCRIPT } else { &SPAN_SUBSCRIPT };
            let end = scan_bash_span(input, bracket, depth, ctx, commands, span, false, Some(&mut subscript))?;
            if bash_evaluates_expansion(&subscript, Evaluation::Arithmetic, ctx) {
                return Err(O::DynamicExecution);
            }
            if let Some(text) = text.as_deref_mut() {
                text.word.push_str(input.slice(index, bracket));
                text.word.push_str(&subscript.word);
                text.word.push(']');
            }
            return Ok(Some(end));
        }
    }
    let opener: Option<&str> = if c == '$' {
        bash_operator(input, index, &["$$", "$((", "$(", "${", "$[", "$'", "$\""])?
    } else if mode == Mode::Word && (c == '<' || c == '>') {
        bash_operator(input, index, if c == '<' { &["<("] } else { &[">("] })?
    } else {
        None
    };
    if opener == Some("$$") {
        let quote_start = index + 2 + input.sticky(&LINE_CONTINUATION_RE, index + 2).unwrap_or(0);
        if input.is(quote_start, '\'') {
            let Some(next_quote) = input.index_of_char('\'', quote_start + 1) else {
                return Err(O::UnterminatedQuote);
            };
            // Zsh and Bash 3.2 inside ${...} can parse $$'...' with ANSI-C escapes while Bash 5/Dash treat $$ as PID.
            if input.slice(quote_start + 1, next_quote).contains('\\') && input.includes("'", next_quote + 1) {
                return Err(O::UnterminatedQuote);
            }
        }
        if let Some(text) = text.as_deref_mut() {
            text.word.push_str("$$");
        }
        return Ok(Some(index + 1));
    }
    if mode == Mode::Word && opener == Some("$'") {
        let Some((value, quote_end)) = bash_ansi_quote(input, index + 1) else {
            return Err(O::UnterminatedQuote);
        };
        let first_quote = input.index_of_char('\'', index + 2).map_or(-1, |v| v as isize);
        // Dash does not support $'...' and closes the single-quoted span at the first `'`, even after `\`.
        if quote_end as isize != first_quote
            && ((ctx.dialect != Dialect::Bash && index >= 2 && input.is(index - 2, '$'))
                || (ctx.dialect == Dialect::Posix
                    && (input.index_of_char('\'', (first_quote + 1) as usize).map_or(-1, |v| v as isize)
                        < quote_end as isize
                        || test(&RE_DOLLAR_QUOTE_POSIX, input.slice_from((first_quote + 1) as usize)))))
        {
            return Err(O::UnterminatedQuote);
        }
        if let Some(text) = text.as_deref_mut() {
            bash_append(text, &value);
        }
        return Ok(Some(quote_end));
    }
    if mode == Mode::Arithmetic && opener == Some("$'") {
        let quote = bash_ansi_quote(input, index + 1);
        let first = input.index_of_char('\'', index + 2);
        match quote {
            Some((value, end)) if Some(end) == first && !test(&RE_ARITH_QUOTE_REJECT, &value) => {
                if let Some(text) = text.as_deref_mut() {
                    bash_append(text, &value);
                }
            }
            _ => return Err(O::InvalidStructure),
        }
        return scan_bash_span(input, index + 2, depth, ctx, commands, &SPAN_ARITHMETIC_QUOTE, allow_bracket, text)
            .map(Some);
    }
    if mode == Mode::Word && c == '\'' {
        let Some(end) = input.index_of_char('\'', index + 1) else {
            return Err(O::UnterminatedQuote);
        };
        if let Some(text) = text.as_deref_mut() {
            bash_append(text, input.slice(index + 1, end));
        }
        return Ok(Some(end));
    }
    if mode == Mode::Arithmetic && c == '\'' {
        return scan_bash_span(input, index + 1, depth, ctx, commands, &SPAN_ARITHMETIC_QUOTE, allow_bracket, text)
            .map(Some);
    }
    if mode == Mode::Word && opener == Some("$\"") {
        return scan_bash_span(input, index + 2, depth, ctx, commands, &SPAN_DOUBLE, allow_bracket, text).map(Some);
    }
    if mode != Mode::Quoted && c == '"' {
        let span = if mode == Mode::Arithmetic { &SPAN_ARITHMETIC_DOUBLE } else { &SPAN_DOUBLE };
        return scan_bash_span(input, index + 1, depth, ctx, commands, span, allow_bracket, text).map(Some);
    }
    let end = if opener == Some("<(") || opener == Some(">(") {
        Some(scan_bash_nested(input, index + 2, depth, ctx, commands, Close::Paren, None)?)
    } else if (opener.is_some() && opener != Some("$'") && opener != Some("$\"")) || c == '`' {
        Some(scan_bash_dollar_or_backtick(
            input,
            index,
            depth,
            ctx,
            commands,
            mode != Mode::Word,
            allow_bracket,
            text.as_deref_mut(),
        )?)
    } else {
        None
    };
    if end.is_none() && c == '$' && (next.is_ascii_alphanumeric() || "_@*#?$!-".contains(next)) {
        if let Some(text) = text.as_deref_mut() {
            text.word.push('$');
        }
        return Ok(Some(index));
    }
    if let (Some(text), Some(end)) = (text, end) {
        text.word.push_str(input.slice(index, end + 1));
    }
    Ok(end)
}

// Zsh flag groups such as ${(e)name} evaluate a value, and a modifier run with ~, such as $^~name or
// ${(f)~name}, globs it, which can run glob qualifier code.
fn zsh_evaluates(input: &Input, index: usize) -> bool {
    let mut at = index + 1;
    if input.starts_with("{\\\n", at) {
        return true;
    }
    if input.is(at, '{') && input.is(at + 1, '(') {
        match input.sticky(&ZSH_SAFE_FLAGS_RE, index) {
            None => return true,
            Some(n) => at = index + n,
        }
    }
    if input.is(at, '{') {
        at += 1;
    }
    input.sticky(&ZSH_GLOB_MODIFIER_RE, at).is_some()
}

// Arithmetic evaluation expands `name[subscript]` again, so a literal expansion in text the shell evaluates as
// arithmetic can run. See upstream for the full rationale.
fn bash_evaluates_expansion(text: &BashText, evaluation: Evaluation, ctx: &mut BashContext) -> bool {
    let combined = format!("{} {}", text.literal, text.output);
    let has_expansion = test(&BASH_EXPANSION_RE, &combined);
    let has_escape = test(&BASH_ESCAPE_RE, &text.literal);
    if has_expansion || has_escape || text.literal.contains(['$', '`']) {
        ctx.bound_dollar = true;
    }
    if has_expansion || has_escape || test(&RE_OPEN_CHARS, &text.literal) {
        ctx.bound_open = true;
    }
    if evaluation == Evaluation::Deferred {
        return false;
    }
    let sink = if evaluation == Evaluation::Arithmetic {
        test(&RE_ARITH_SINK, &text.word)
    } else {
        text.word.contains('$') || combined.contains('[')
    };
    if sink {
        ctx.variable_sink = true;
    }
    if evaluation == Evaluation::Arithmetic {
        return has_expansion;
    }
    combined.contains('[') && has_expansion
}

// Scans a subscript after its `[`; indexed arrays evaluate it as arithmetic.
fn scan_bash_subscript(
    input: &Input,
    start: usize,
    depth: usize,
    ctx: &mut BashContext,
    commands: &mut Vec<CmdRef>,
    span: &Span,
) -> R<usize> {
    let mut text = BashText::default();
    let end = scan_bash_span(input, start, depth, ctx, commands, span, false, Some(&mut text))?;
    if bash_evaluates_expansion(&text, Evaluation::Arithmetic, ctx) {
        return Err(O::DynamicExecution);
    }
    Ok(end)
}

// Scans to the span's unnested close character and returns its index; a span without one runs to the end.
#[allow(clippy::too_many_arguments)]
fn scan_bash_span(
    input: &Input,
    start: usize,
    depth: usize,
    ctx: &mut BashContext,
    commands: &mut Vec<CmdRef>,
    span: &Span,
    allow_bracket: bool,
    mut text: Option<&mut BashText>,
) -> R<usize> {
    let mut nesting: usize = 0;
    let len = input.len();
    let mut index = start as isize - 1;
    loop {
        index += 1;
        if index as usize >= len {
            break;
        }
        let i = index as usize;
        ctx.remaining -= 1;
        if ctx.remaining < 0 {
            return Err(O::InvalidStructure);
        }
        let c = input.chars[i];
        if Some(c) == span.close && nesting == 0 {
            return Ok(i);
        }
        // A `#` after a digit is a base prefix, and after `$` the parameter $#.
        if span.reject.contains(c)
            && !(c == '#' && input.ati(i as isize - 1).is_some_and(|p| p.is_ascii_digit() || p == '$'))
        {
            return Err(O::InvalidStructure);
        }
        if Some(c) == span.open {
            nesting += 1;
            if nesting + depth > MAX_SUBSTITUTION_DEPTH {
                return Err(O::InvalidStructure);
            }
        }
        if Some(c) == span.close {
            nesting = nesting.wrapping_sub(1);
        }
        let unit = if Some(c) == span.open || Some(c) == span.close {
            None
        } else {
            scan_bash_unit(input, i, depth, ctx, commands, span.mode, allow_bracket, text.as_deref_mut())?
        };
        if let Some(unit) = unit {
            index = unit as isize;
        } else if c == '\\' && (input.at(i + 1) == span.open || input.at(i + 1) == span.close) {
            if let Some(text) = text.as_deref_mut() {
                bash_append(text, input.slice(i, i + 2));
            }
            index += 1;
        } else if let Some(text) = text.as_deref_mut() {
            bash_append_char(text, c);
        }
    }
    if span.close.is_some() { Err(O::UnterminatedQuote) } else { Ok(len) }
}

// Scans a nested list such as a command or process substitution and returns its closing index.
fn scan_bash_nested(
    input: &Input,
    start: usize,
    depth: usize,
    ctx: &mut BashContext,
    commands: &mut Vec<CmdRef>,
    close: Close,
    text: Option<&mut BashText>,
) -> R<usize> {
    let (nested, end) = scan_bash(input, start, depth + 1, ctx, Some(close), None, text)?;
    commands.extend(nested);
    Ok(end)
}

// Returns the first listed operator at index. Shells disagree about operators split by line continuations.
fn bash_operator(input: &Input, index: usize, operators: &[&'static str]) -> R<Option<&'static str>> {
    let mut text = String::new();
    let mut count = 0;
    let mut cursor = index;
    while count < 3 && cursor < input.len() {
        if input.starts_with("\\\n", cursor) {
            cursor += 1;
        } else {
            text.push(input.chars[cursor]);
            count += 1;
        }
        cursor += 1;
    }
    let operator = operators.iter().copied().find(|candidate| text.starts_with(candidate));
    if let Some(operator) = operator {
        if !input.starts_with(operator, index) {
            return Err(O::InvalidStructure);
        }
    }
    Ok(operator)
}

#[allow(clippy::too_many_arguments)]
fn scan_bash_dollar_or_backtick(
    input: &Input,
    start: usize,
    depth: usize,
    ctx: &mut BashContext,
    commands: &mut Vec<CmdRef>,
    quoted: bool,
    allow_bracket: bool,
    text: Option<&mut BashText>,
) -> R<usize> {
    if depth >= MAX_SUBSTITUTION_DEPTH {
        return Err(O::CommandSubstitution);
    }
    if input.is(start, '`') {
        return scan_bash_backtick(input, start, depth + 1, ctx, commands, quoted, text);
    }
    if input.starts_with("$((", start) {
        if let Some(end) = scan_bash_arithmetic(input, start + 3, depth + 1, ctx, commands, &SPAN_ARITHMETIC)? {
            return Ok(end);
        }
    }
    if input.starts_with("$(", start) {
        return scan_bash_nested(input, start + 2, depth, ctx, commands, Close::Paren, text);
    }
    if let Some(n) = input.sticky(&NOFORK_OPEN_RE, start) {
        return scan_bash_nested(input, start + n, depth, ctx, commands, Close::Nofork, text);
    }
    if input.starts_with("${", start) {
        // Literal text in parameter words reaches the enclosing word's value.
        let mut parameter = BashText::default();
        let end = scan_bash_parameter(input, start + 2, depth + 1, ctx, commands, quoted, &mut parameter);
        // ${!name} evaluates the subscript in its name.
        if test(&RE_BANG_PARAM, input.slice_from(start + 2)) {
            ctx.variable_sink = true;
            if bash_evaluates_expansion(&parameter, Evaluation::Binding, ctx) {
                return Err(O::DynamicExecution);
            }
        }
        if let Some(text) = text {
            text.literal.push_str(&parameter.literal);
            text.output.push_str(&parameter.output);
        }
        return end;
    }
    // Dash leaves `$[` literal, which splits words differently; Bash and Zsh always read arithmetic.
    if !allow_bracket && ctx.dialect == Dialect::Posix {
        return Err(O::CommandSubstitution);
    }
    let mut expression = BashText::default();
    let end = scan_bash_span(input, start + 2, depth + 1, ctx, commands, &SPAN_BRACKET_ARITHMETIC, true, Some(&mut expression));
    if end.is_ok() && bash_evaluates_expansion(&expression, Evaluation::Arithmetic, ctx) {
        return Err(O::DynamicExecution);
    }
    end
}

// Scans the text of ((...)) after its opening parentheses. Returns None when the text closes with a single
// parenthesis, which makes it a nested subshell instead.
fn scan_bash_arithmetic(
    input: &Input,
    start: usize,
    depth: usize,
    ctx: &mut BashContext,
    commands: &mut Vec<CmdRef>,
    span: &Span,
) -> R<Option<usize>> {
    let mut arithmetic: Vec<CmdRef> = vec![];
    let mut text = BashText::default();
    let Ok(end) = scan_bash_span(input, start, depth, ctx, &mut arithmetic, span, true, Some(&mut text)) else {
        return Err(O::InvalidStructure);
    };
    let close = bash_operator(input, end, &["))"])?;
    if close.is_none() {
        return Ok(None);
    }
    if bash_evaluates_expansion(&text, Evaluation::Arithmetic, ctx) {
        return Err(O::DynamicExecution);
    }
    commands.extend(arithmetic);
    Ok(Some(end + 1))
}

fn scan_bash_backtick(
    input: &Input,
    start: usize,
    depth: usize,
    ctx: &mut BashContext,
    commands: &mut Vec<CmdRef>,
    quoted: bool,
    text: Option<&mut BashText>,
) -> R<usize> {
    let mut source = String::new();
    let mut index = start + 1;
    while index < input.len() {
        ctx.remaining -= 1;
        if ctx.remaining < 0 {
            return Err(O::InvalidStructure);
        }
        let c = input.chars[index];
        if c == '`' {
            let inner = Input::new(&source);
            return match scan_bash(&inner, 0, depth, ctx, None, None, text) {
                Err(_) => Err(O::CommandSubstitution),
                Ok((inner, _)) => {
                    commands.extend(inner);
                    Ok(index)
                }
            };
        }
        let next = input.at(index + 1).unwrap_or('\0');
        if c == '\\' && ("$`\\\n".contains(next) || (quoted && next == '"')) {
            index += 1;
            if next != '\n' {
                source.push(next);
            }
            index += 1;
            continue;
        }
        source.push(c);
        index += 1;
    }
    Err(O::CommandSubstitution)
}

fn scan_bash_parameter(
    input: &Input,
    start: usize,
    depth: usize,
    ctx: &mut BashContext,
    commands: &mut Vec<CmdRef>,
    quoted: bool,
    text: &mut BashText,
) -> R<usize> {
    let name_start = start + input.sticky(&PARAMETER_PREFIX_RE, start).unwrap_or(0);
    // The operands of ${name:offset:length} are arithmetic, and ${name:=word} or ${name=word} binds a variable.
    let mut colon: isize = match input.sticky(&PARAMETER_NAME_RE, name_start) {
        Some(n) => (name_start + n) as isize,
        None => -1,
    };
    let name = if colon > name_start as isize {
        input.slice(name_start, colon as usize).replace("\\\n", "")
    } else {
        String::new()
    };
    if ctx.dialect != Dialect::Bash
        && (test(&RE_NULLCMD, &name)
            || (test(&RE_ZSH_SINK_NAME, &name) && input.ati(colon).is_none_or(|c| "[=:".contains(c))))
    {
        return Err(O::DynamicExecution);
    }
    if name == "PS4" {
        ctx.variable_sink = true;
    }
    let mut evaluation: Option<Evaluation> = None;
    let mut word_offset = 0;
    let mut literal_offset = 0;
    let len = input.len();
    let mut index = start as isize - 1;
    loop {
        index += 1;
        if index as usize >= len {
            break;
        }
        let i = index as usize;
        ctx.remaining -= 1;
        if ctx.remaining < 0 {
            return Err(O::InvalidStructure);
        }
        let c = input.chars[i];
        if c == '}' {
            if let Some(evaluation) = evaluation {
                let slice = BashText {
                    word: text.word.get(word_offset..).unwrap_or("").to_string(),
                    literal: text.literal.get(literal_offset..).unwrap_or("").to_string(),
                    output: text.output.clone(),
                };
                if bash_evaluates_expansion(&slice, evaluation, ctx) {
                    return Err(O::DynamicExecution);
                }
            }
            return Ok(i);
        }
        if index == colon && evaluation.is_none() {
            if c == ':' && !input.at(i + 1).is_none_or(|n| "-=?+".contains(n)) {
                evaluation = Some(Evaluation::Arithmetic);
            }
            if c == '=' || (c == ':' && input.is(i + 1, '=')) {
                evaluation = Some(Evaluation::Deferred);
            }
            if evaluation.is_some() {
                word_offset = text.word.len();
                literal_offset = text.literal.len();
            }
        }
        // Bash ${name@P} evaluates the value as prompt text.
        if c == '@' && input.sticky(&PARAMETER_PROMPT_RE, i).is_some() {
            return Err(O::DynamicExecution);
        }
        // Zsh globs an unquoted parameter's words, where a parenthesized group can be a glob qualifier.
        if !quoted && ctx.dialect != Dialect::Bash && c == '(' && i > start {
            return Err(O::DynamicExecution);
        }
        if quoted && c == '\'' && input.ati(i as isize - 1) == Some('$') {
            let Some((value, _)) = bash_ansi_quote(input, i) else {
                return Err(O::UnterminatedQuote);
            };
            text.literal.push_str(&value);
        }
        let subscript = index == colon && c == '[';
        let unit = if subscript {
            Some(scan_bash_subscript(input, i + 1, depth, ctx, commands, &SPAN_SUBSCRIPT)?)
        } else if quoted && c == '\'' {
            Some(scan_bash_span(input, i + 1, depth, ctx, commands, &SPAN_PARAMETER_QUOTE, false, Some(text))?)
        } else if quoted && c == '"' {
            Some(scan_bash_span(input, i + 1, depth, ctx, commands, &SPAN_DOUBLE, false, Some(text))?)
        } else {
            let mode = if quoted { Mode::Quoted } else { Mode::Word };
            scan_bash_unit(input, i, depth, ctx, commands, mode, false, Some(text))?
        };
        // Bash 3.2 ends a double-quoted parameter at an unquoted `}` inside `$(...)`.
        if let Some(unit) = unit {
            if quoted && input.starts_with("$(", i) && input.slice(i, unit + 1).contains('}') {
                return Err(O::CommandSubstitution);
            }
        }
        if subscript || (i == name_start && input.starts_with("${", i) && unit.is_some()) {
            let after = unit.map_or(0, |u| u + 1);
            colon = (after + input.sticky(&LINE_CONTINUATION_RE, after).unwrap_or(0)) as isize;
        }
        match unit {
            Some(unit) => index = unit as isize,
            None => bash_append_char(text, c),
        }
    }
    Err(O::CommandSubstitution)
}

fn scan_bash_conditional(
    input: &Input,
    start: usize,
    depth: usize,
    ctx: &mut BashContext,
    commands: &mut Vec<CmdRef>,
) -> R<usize> {
    let mut words: Vec<BashText> = vec![];
    let mut text = BashText::default();
    let mut word_started = false;
    let mut has_word = false;
    let mut paren_depth: i64 = 0;
    let mut pattern_paren_depth: i64 = 0;
    fn flush(words: &mut Vec<BashText>, text: &mut BashText, has_word: &mut bool) {
        if !*has_word {
            return;
        }
        words.push(std::mem::take(text));
        *has_word = false;
    }
    let len = input.len();
    let mut index = start as isize - 1;
    loop {
        index += 1;
        if index as usize >= len {
            break;
        }
        let i = index as usize;
        ctx.remaining -= 1;
        if ctx.remaining < 0 {
            return Err(O::InvalidStructure);
        }
        let c = input.chars[i];
        if c == '\\' && input.is(i + 1, '\n') {
            index += 1;
            continue;
        }
        if !word_started {
            flush(&mut words, &mut text, &mut has_word);
        }
        let close = if !word_started && c == ']' { bash_operator(input, i, &["]]"])? } else { None };
        if close.is_some() && input.sticky(&BRACE_CLOSE_AHEAD_RE, i + 2).is_some() {
            if paren_depth != 0 {
                return Err(O::InvalidStructure);
            }
            // -v names a variable, -t in Zsh evaluates the descriptor, and arithmetic comparisons evaluate both operands.
            let dialect = ctx.dialect;
            let evaluated = (0..words.len()).any(|at| {
                let prev = if at > 0 { Some(words[at - 1].word.as_str()) } else { None };
                let next = words.get(at + 1).map(|w| w.word.as_str());
                (prev == Some("-v") && bash_evaluates_expansion(&words[at], Evaluation::Binding, ctx))
                    || (((dialect != Dialect::Bash && prev == Some("-t"))
                        || has(BASH_ARITHMETIC_COMPARISONS, prev.unwrap_or(""))
                        || has(BASH_ARITHMETIC_COMPARISONS, next.unwrap_or("")))
                        && bash_evaluates_expansion(&words[at], Evaluation::Arithmetic, ctx))
            });
            if evaluated {
                return Err(O::DynamicExecution);
            }
            return Ok(i + 1);
        }
        if !word_started && c == '#' {
            let Some(newline) = input.index_of_char('\n', i) else {
                return Err(O::InvalidStructure);
            };
            index = newline as isize;
            continue;
        }
        if c == ' ' || c == '\t' || c == '\n' {
            if pattern_paren_depth == 0 {
                word_started = false;
            }
            continue;
        }
        if c == ';' {
            return Err(O::InvalidStructure);
        }
        if c == '(' || c == ')' {
            if c == ')' && paren_depth == 0 {
                return Err(O::InvalidStructure);
            }
            let in_pattern = pattern_paren_depth > 0
                || (c == '('
                    && ["=", "==", "!=", "=~"].contains(&words.last().map(|w| w.word.as_str()).unwrap_or("")));
            paren_depth += if c == '(' { 1 } else { -1 };
            if in_pattern {
                pattern_paren_depth += if c == '(' { 1 } else { -1 };
                word_started = true;
                has_word = true;
                bash_append_char(&mut text, c);
                continue;
            }
            flush(&mut words, &mut text, &mut has_word);
            words.push(BashText { word: c.to_string(), ..Default::default() });
            word_started = false;
            continue;
        }
        let unit = scan_bash_unit(input, i, depth, ctx, commands, Mode::Word, true, Some(&mut text))?;
        if let Some(unit) = unit {
            index = unit as isize;
        }
        if unit.is_none() && pattern_paren_depth == 0 && "&|<>".contains(c) {
            flush(&mut words, &mut text, &mut has_word);
            words.push(BashText { word: c.to_string(), ..Default::default() });
            word_started = false;
            continue;
        }
        word_started = true;
        has_word = true;
        if unit.is_none() {
            bash_append_char(&mut text, c);
        }
    }
    Err(O::InvalidStructure)
}

fn scan_bash_array_or_pattern(
    input: &Input,
    start: usize,
    depth: usize,
    ctx: &mut BashContext,
    commands: &mut Vec<CmdRef>,
    mode: ListMode,
) -> R<usize> {
    if depth > MAX_SUBSTITUTION_DEPTH {
        return Err(O::CommandSubstitution);
    }
    // Array elements bind variables, so their decoded text is a subscript sink.
    let mut text = if mode == ListMode::Array { Some(BashText::default()) } else { None };
    let mut word_started = false;
    let len = input.len();
    let mut index = start as isize;
    loop {
        index += 1;
        if index as usize >= len {
            break;
        }
        let i = index as usize;
        ctx.remaining -= 1;
        if ctx.remaining < 0 {
            return Err(O::InvalidStructure);
        }
        let c = input.chars[i];
        if c == '\\' && input.is(i + 1, '\n') {
            index += 1;
            continue;
        }
        if let Some(text) = text.as_mut() {
            if c == ')' || c == ' ' || c == '\t' || c == '\n' {
                bash_evaluates_expansion(text, Evaluation::Deferred, ctx);
                text.literal.clear();
                text.output.clear();
            }
        }
        if c == ')' {
            return Ok(i);
        }
        if mode == ListMode::Array && !word_started && c == '#' {
            let Some(newline) = input.index_of_char('\n', i) else {
                return Err(O::CommandSubstitution);
            };
            index = newline as isize;
            continue;
        }
        if c == ' ' || c == '\t' || (mode == ListMode::Array && c == '\n') {
            word_started = false;
            continue;
        }
        // Array elements are globbed, where a parenthesized group can be a Zsh glob qualifier.
        if c == '\n' || c == ';' || c == '&' || (mode == ListMode::Array && (c == '|' || c == '(')) {
            return Err(O::CommandSubstitution);
        }
        word_started = true;
        let unit = if mode == ListMode::Array && c == '=' && input.is(i + 1, '(') {
            Some(scan_bash_nested(input, i + 2, depth, ctx, commands, Close::Paren, None)?)
        } else if c == '(' {
            Some(scan_bash_array_or_pattern(input, i, depth + 1, ctx, commands, mode)?)
        } else if c == '[' {
            Some(scan_bash_subscript(input, i + 1, depth, ctx, commands, &SPAN_SUBSCRIPT)?)
        } else {
            scan_bash_unit(input, i, depth, ctx, commands, Mode::Word, false, text.as_mut())?
        };
        match unit {
            Some(unit) => index = unit as isize,
            None => {
                if let Some(text) = text.as_mut() {
                    text.literal.push(c);
                }
            }
        }
    }
    Err(O::CommandSubstitution)
}

fn ansi_escape(c: char) -> Option<char> {
    Some(match c {
        'a' => '\x07',
        'b' => '\x08',
        'e' | 'E' => '\x1b',
        'f' => '\x0c',
        'n' => '\n',
        'r' => '\r',
        't' => '\t',
        'v' => '\x0b',
        '\\' => '\\',
        '\'' => '\'',
        '"' => '"',
        '?' => '?',
        _ => return None,
    })
}

fn bash_ansi_quote(input: &Input, start: usize) -> Option<(String, usize)> {
    let mut value = String::new();
    let mut index = start + 1;
    while index < input.len() {
        let c = input.chars[index];
        if c == '\'' {
            return Some((value, index));
        }
        if c != '\\' {
            value.push(c);
            index += 1;
            continue;
        }
        index += 1;
        let escaped = input.at(index)?;
        if let Some(replacement) = ansi_escape(escaped) {
            value.push(replacement);
            index += 1;
            continue;
        }
        let octal = ('0'..='7').contains(&escaped);
        let digits: String = if escaped == 'x' {
            input.slice(index + 1, index + 3).chars().take_while(char::is_ascii_hexdigit).collect()
        } else if octal {
            input.slice(index, index + 3).chars().take_while(|c| ('0'..='7').contains(c)).collect()
        } else {
            String::new()
        };
        if digits.is_empty() {
            return None;
        }
        let point = u32::from_str_radix(&digits, if octal { 8 } else { 16 }).ok()?;
        if point == 0 {
            return None;
        }
        value.push(char::from_u32(point)?);
        index += digits.chars().count() - usize::from(octal);
        index += 1;
    }
    None
}

fn bash_heredoc_delimiter(input: &Input, start: usize, tabs: bool) -> Option<BashHeredoc> {
    let mut delimiter = String::new();
    let mut dash_delimiter: Option<String> = None;
    let mut quoted = false;
    let mut quote: Option<char> = None;
    let mut end = start;
    let mut started = false;
    let make = |delimiter: String, dash_delimiter: Option<String>, quoted: bool, end: usize| BashHeredoc {
        delimiter,
        dash_delimiter,
        quoted,
        tabs,
        binding: None,
        command: None,
        start: None,
        end,
    };
    let mut index = start;
    while index < input.len() {
        let c = input.chars[index];
        if !started && (c == ' ' || c == '\t') {
            index += 1;
            continue;
        }
        if !started && c == '#' {
            return None;
        }
        if quote.is_none() && " \t\n;&|()<>".contains(c) {
            return if started { Some(make(delimiter, dash_delimiter, quoted, end)) } else { None };
        }
        if quote.is_none() && c == '$' && input.starts_with("\\\n", index + 1) {
            return None;
        }
        // Dash retains $ in $'...' heredoc delimiters while Bash and Zsh strip $.
        if quote.is_none() && input.starts_with("$'", index) {
            let (value, literal_end) = bash_ansi_quote(input, index + 1)?;
            if Some(literal_end) != input.index_of_char('\'', index + 2) {
                return None;
            }
            let base = dash_delimiter.take().unwrap_or_else(|| delimiter.clone());
            dash_delimiter = Some(format!("{base}${}", input.slice(index + 2, literal_end)));
            delimiter.push_str(&value);
            quoted = true;
            started = true;
            index = literal_end;
            end = index;
            index += 1;
            continue;
        }
        // Bash strips $ from $"..." heredoc delimiters while Zsh and Dash retain $.
        if quote.is_none() && input.starts_with("$\"", index) {
            return None;
        }
        if Some(c) == quote {
            quote = None;
            end = index;
            index += 1;
            continue;
        }
        if c == '\\' && quote != Some('\'') {
            let next = input.at(index + 1)?;
            if next == '\n' {
                index += 2;
                continue;
            }
            // Double quotes only remove escapes for shell-special characters.
            if quote.is_none() || "$`\"\\".contains(next) {
                quoted = true;
                started = true;
                index += 1;
                let escaped = input.chars[index];
                if let Some(dash) = dash_delimiter.as_mut() {
                    dash.push(escaped);
                }
                delimiter.push(escaped);
                end = index;
                index += 1;
                continue;
            }
        }
        if quote.is_none() && (c == '\'' || c == '"') {
            quote = Some(c);
            quoted = true;
            started = true;
            end = index;
            index += 1;
            continue;
        }
        if let Some(dash) = dash_delimiter.as_mut() {
            dash.push(c);
        }
        delimiter.push(c);
        started = true;
        end = index;
        index += 1;
    }
    if started && quote.is_none() { Some(make(delimiter, dash_delimiter, quoted, end)) } else { None }
}

fn bash_heredoc(input: &Input, start: usize, delimiter: &BashHeredoc) -> Option<(String, usize)> {
    let body_start = start;
    let mut start = start;
    let mut line_start = start;
    let mut line = String::new();
    // Backslashes ending the joined line; an odd count continues it.
    let mut backslashes: usize = 0;
    let len = input.len();
    let max_length = utf16_len(&delimiter.delimiter).max(delimiter.dash_delimiter.as_deref().map_or(0, utf16_len));
    let mut index = start;
    while index <= len {
        if index < len && input.chars[index] != '\n' {
            index += 1;
            continue;
        }
        let raw = input.slice(start, index);
        let text = if delimiter.tabs && start == line_start { raw.trim_start_matches('\t') } else { raw };
        let text_chars: Vec<char> = text.chars().collect();
        let mut run = 0;
        while run < text_chars.len() && text_chars[text_chars.len() - 1 - run] == '\\' {
            run += 1;
        }
        backslashes = if run == text_chars.len() { backslashes + run } else { run };
        let continued = !delimiter.quoted && backslashes % 2 == 1 && index < len;
        // Joined lines only grow, so text past the delimiter's length cannot change the comparison.
        if utf16_len(&line) <= max_length {
            if continued {
                line.extend(&text_chars[..text_chars.len() - 1]);
            } else {
                line.push_str(text);
            }
        }
        if continued {
            backslashes -= 1;
            start = index + 1;
            index += 1;
            continue;
        }
        if delimiter.dash_delimiter.as_deref() == Some(line.as_str()) {
            return None;
        }
        if line == delimiter.delimiter {
            // Dash does not join backslash-continued delimiter lines; fail closed if a physical delimiter follows.
            if start > line_start && input.includes(&delimiter.delimiter, index + 1) {
                return None;
            }
            return Some((input.slice(body_start, line_start).to_string(), index));
        }
        line.clear();
        backslashes = 0;
        start = index + 1;
        line_start = start;
        index += 1;
    }
    None
}

// ---------------------------------------------------------------------------------------------
// PowerShell

const PS_WS_NO_NEWLINE: &str =
    r"[\t\x0B\x0C \x{A0}\x{1680}\x{2000}-\x{200A}\x{2028}\x{2029}\x{202F}\x{205F}\x{3000}\x{FEFF}]";

regex!(POWERSHELL_STOP_PARSING_RE, sticky r"--%(?=$|[\s;|&(){}])");
regex!(POWERSHELL_PARAMETER_COLON_RE, r"^-[\p{L}\p{Nl}_?][\p{L}\p{Nl}\d_?-]*:$");
regex!(POWERSHELL_MERGE_REDIRECT_RE, sticky r"\d>&");
regex!(POWERSHELL_LABEL_RE, sticky r"(?i):[A-Za-z_]\w*(?=\s*(?:while|for|foreach|do|switch)(?=$|[\s&(),;{|}]))");
regex!(POWERSHELL_KEYWORD_RE, sticky r"[A-Za-z]+(?=$|[\s&(),;{|}])");
regex!(POWERSHELL_FOREACH_RE, sticky r"(?i)foreach\s*\(");
regex!(POWERSHELL_PARAM_RE, sticky r"(?i)param\s*\(");
regex!(POWERSHELL_IN_RE, sticky r"(?i)in(?=$|[\s&(),;{|}])");
regex!(POWERSHELL_AT_OPENER_RE, sticky r#"[({'"\p{L}\p{Nl}\d_?]"#);
regex!(POWERSHELL_INDEXABLE_VAR_RE, sticky r"(?::[\p{L}\p{Nl}\d_?]+|[\p{L}\p{Nl}\d_][\p{L}\p{Nl}\d_?]*)");
static POWERSHELL_HERE_HEADER_RE: LazyLock<Regex> =
    LazyLock::new(|| sticky(&format!(r#"@['"]{PS_WS_NO_NEWLINE}*(?:\r\n|\r|\n)"#)));
regex!(POWERSHELL_UNICODE_ESCAPE_RE, sticky r"(?i)u\{([0-9a-f]{1,6})\}");
regex!(
    POWERSHELL_EXPRESSION_RE,
    sticky r"(?i)(?:!*(?:\+\+|--|[+-])?\$(?:\{|(?:\?|\$|\^)(?=$|[\s;|&(){},#>])|:[\p{L}\p{Nl}\d_?]+|[\p{L}\p{Nl}\d_][\p{L}\p{Nl}\d_?]*(?::[\p{L}\p{Nl}\d_?]+)?)|!(?![\p{L}\p{Nl}_.\d])|\+(?![=\p{L}\p{Nl}_.\d])|--(?!=)|-(?![=\p{L}\p{Nl}\d_?.])|,(?!\s*$)|!*[+-]?(?:0[xX][\da-fA-F]+|0[bB][01]+|(?:\d+(?:\.(?!\.)\d*|(?!\.(?!\.)))|\.\d+)(?:[eE][+-]?\d+)?)(?:[dDlLnN]|[uU][sSyYlL]?|[sSyY])?(?:[kKmMgGtTpP][bB])?(?=$|[\s!#%&()*+,\-/.;<=>\]|{}])|-(?:not|bnot|join|[ic]?split)(?![\p{L}\p{Nl}]))"
);
regex!(RE_PS_TYPE_WORD, r"^[\p{L}\p{Nl}_][\p{L}\p{Nl}\d_.#+`\\]*$");
regex!(RE_PS_VARIABLE_CHAR, r"^[\p{L}\p{Nl}\d_:?]$");
regex!(RE_PS_MEMBER_CHAR, r"^[\p{L}\p{Nl}\d_]$");
regex!(RE_PS_REDIRECT, r"^(?:(?:[1-6]|\*)?>>?|[2-6*]>&1)$");

fn ps_escape_char(c: char) -> Option<char> {
    Some(match c {
        '0' => '\0',
        'a' => '\x07',
        'b' => '\x08',
        'e' => '\x1b',
        'f' => '\x0c',
        'n' => '\n',
        'r' => '\r',
        't' => '\t',
        'v' => '\x0b',
        _ => return None,
    })
}

struct Budget {
    remaining: i64,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Clause {
    Member,
    Block,
    If,
    Try,
    Clause,
    Switch,
    Paren,
    Do,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Indexable {
    No,
    Variable,
    Closed,
    Member,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Hash {
    False,
    True,
    Clause,
    Switch,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum After {
    Do,
    If,
    Try,
}

type Skipped = HashSet<usize>;

fn skip(skipped: Option<&Skipped>, index: usize) -> bool {
    skipped.is_some_and(|s| s.contains(&index))
}

fn ends_dot_colon(word: &str) -> bool {
    word.ends_with('.') || word.ends_with(':')
}

fn space_at(input: &Input, index: usize) -> bool {
    input.at(index).is_some_and(js_space)
}

fn char_test(regex: &Regex, c: char) -> bool {
    let mut buffer = [0u8; 4];
    test(regex, c.encode_utf8(&mut buffer))
}

pub fn scan_powershell(input: &str) -> ScanOutcome {
    if utf16_len(input) > MAX_INPUT_LENGTH {
        return ScanOutcome::Opaque(O::InvalidStructure);
    }
    // PowerShell's Unicode quotes, dashes, and whitespace differ from JavaScript's token rules.
    if input.chars().any(|c| {
        matches!(c, '\0' | '\u{85}' | '\u{2013}'..='\u{2015}' | '\u{2018}'..='\u{201e}' | '\u{feff}')
    }) {
        return ScanOutcome::Opaque(O::InvalidStructure);
    }
    let input = Input::new(input);
    let mut budget = Budget { remaining: (MAX_INPUT_LENGTH * MAX_SUBSTITUTION_DEPTH) as i64 };
    match scan_powershell_list(&input, 0, 0, &mut budget, None, Hash::False, None) {
        Err(reason) => ScanOutcome::Opaque(reason),
        Ok((commands, _)) => ScanOutcome::Scanned(commands),
    }
}

struct Ps<'a> {
    input: &'a Input,
    commands: Vec<Command>,
    nested_commands: Vec<Command>,
    words: Vec<String>,
    raw_words: Vec<String>,
    word_ends: Vec<usize>,
    initial_expression: bool,
    initial_clause: Option<Clause>,
    segment: usize,
    word: String,
    started: bool,
    word_start: usize,
    expression: bool,
    clause: Option<Clause>,
    after: Option<After>,
    indexable: Indexable,
    stop_parsing: bool,
    command_end: usize,
    statement_head: bool,
    invalid: bool,
    redirect_target: bool,
    dangling: bool,
    invocation: bool,
}

impl Ps<'_> {
    fn finish_word(&mut self, end: usize) {
        if !self.started {
            return;
        }
        // Generic tokens can spell stop-parsing with escapes or embedded quotes; literal strings cannot.
        let last_raw = self.raw_words.last().map(String::as_str).unwrap_or("");
        if self.word == "--%"
            && !self.words.is_empty()
            && !self.expression
            && self.clause.is_none()
            && !self.redirect_target
            && !matches!(self.input.at(self.word_start), Some('\'' | '"'))
            && !last_raw.ends_with(',')
            && !test(&POWERSHELL_PARAMETER_COLON_RE, last_raw)
        {
            self.stop_parsing = true;
        }
        if !self.redirect_target {
            if self.words.is_empty() && !self.invocation && !self.expression {
                self.segment = self.word_start;
            }
            self.words.push(std::mem::take(&mut self.word));
            self.raw_words.push(self.input.slice(self.word_start, end).to_string());
            self.word_ends.push(end);
            self.dangling = false;
        }
        self.command_end = end;
        self.redirect_target = false;
        self.word.clear();
        self.started = false;
    }

    fn finish_command(&mut self, end: usize, required: bool) {
        self.finish_word(end);
        if !self.words.is_empty() && !self.expression && self.clause.is_none() {
            let input = self.input;
            let resource = js_trim_end(input.slice(self.segment, self.command_end)).to_string();
            let base = input.byte[self.segment.min(input.len())];
            let word_ends = self.word_ends.iter().map(|&value| input.byte[value.min(input.len())].saturating_sub(base)).collect();
            self.commands.push(Command {
                resource,
                words: self.words.clone(),
                raw_words: self.raw_words.clone(),
                word_ends: Some(word_ends),
                statement_head: self.statement_head && !self.invocation,
                declaration: false,
                redirect_word_count: None,
            });
        } else if (!self.expression && self.clause.is_none() && self.invocation)
            || (required && self.words.is_empty() && self.nested_commands.is_empty())
            || (self.clause.is_some() && self.clause != Some(Clause::Member))
        {
            self.invalid = true;
        }
        if self.redirect_target {
            self.invalid = true;
        }
        let nested = std::mem::take(&mut self.nested_commands);
        self.commands.extend(nested);
        self.words.clear();
        self.raw_words.clear();
        self.word_ends.clear();
        self.redirect_target = false;
        self.invocation = false;
        self.expression = self.initial_expression;
        self.clause = self.initial_clause;
        self.after = None;
        self.indexable = Indexable::No;
        self.stop_parsing = false;
    }

    fn previous(&self) -> String {
        if self.started { self.word.clone() } else { self.words.last().cloned().unwrap_or_default() }
    }
}

fn scan_powershell_list(
    input: &Input,
    start: usize,
    depth: usize,
    budget: &mut Budget,
    close: Option<char>,
    hash: Hash,
    skipped: Option<&Skipped>,
) -> R<(Vec<Command>, usize)> {
    if depth >= MAX_SUBSTITUTION_DEPTH || budget.remaining < 0 {
        return Err(O::InvalidStructure);
    }
    let initial_expression = hash == Hash::True || hash == Hash::Clause;
    let initial_clause = if hash == Hash::Clause || hash == Hash::Switch { Some(Clause::Member) } else { None };
    let mut s = Ps {
        input,
        commands: vec![],
        nested_commands: vec![],
        words: vec![],
        raw_words: vec![],
        word_ends: vec![],
        initial_expression,
        initial_clause,
        segment: start,
        word: String::new(),
        started: false,
        word_start: start,
        expression: initial_expression,
        clause: initial_clause,
        after: None,
        indexable: Indexable::No,
        stop_parsing: false,
        command_end: start,
        statement_head: true,
        invalid: false,
        redirect_target: false,
        dangling: false,
        invocation: false,
    };
    let mut quote: Option<char> = None;
    let mut standalone = false;
    let len = input.len();
    let mut index = start as isize - 1;
    loop {
        index += 1;
        if index as usize >= len {
            break;
        }
        let i = index as usize;
        budget.remaining -= 1;
        if budget.remaining < 0 {
            return Err(O::InvalidStructure);
        }
        if skip(skipped, i) {
            continue;
        }
        let c = input.chars[i];
        let next = input.at(i + 1);
        let in_expression = s.expression && !s.redirect_target;
        let at_boundary = !s.started || (!in_expression && s.word.ends_with(','));
        if !s.started {
            s.word_start = i;
        }
        if s.stop_parsing {
            let stop = powershell_stop_parsing(input, i, skipped);
            let raw = input.slice(i, stop);
            let text = js_trim(raw);
            if !text.is_empty() {
                let offset = raw.chars().position(|c| !js_space(c)).unwrap_or(0);
                s.word_start = i + offset;
                s.word = text.to_string();
                s.started = true;
                let end = s.word_start + text.chars().count();
                s.finish_word(end);
            }
            s.stop_parsing = false;
            index = stop as isize - 1;
            continue;
        }
        if let Some(q) = quote {
            let mut next_quote = i + 1;
            while skip(skipped, next_quote) {
                next_quote += 1;
            }
            if c == q && input.at(next_quote) == Some(q) {
                s.word.push(q);
                index = next_quote as isize;
            } else if c == q {
                quote = None;
                if standalone {
                    s.finish_word(i + 1);
                    s.indexable = Indexable::Closed;
                }
            } else if q == '"' && c == '`' {
                let Some((value, end)) = powershell_escape(input, i, false, skipped) else {
                    return Err(O::UnterminatedEscape);
                };
                s.word.push_str(&value);
                index = end as isize;
            } else if q == '"' && c == '$' && next == Some('{') {
                let Some(end) = powershell_braced_variable(input, i, skipped) else {
                    return Err(O::InvalidStructure);
                };
                s.word.push_str(input.slice(i, end + 1));
                index = end as isize;
            } else if q == '"' && c == '$' && next == Some('(') {
                let Some((sub_skipped, raw_end)) = powershell_sub_expression(input, i + 2, false, budget, skipped) else {
                    return Err(O::InvalidStructure);
                };
                let (commands, result_end) =
                    scan_powershell_list(input, i + 2, depth + 1, budget, Some(')'), Hash::False, Some(&sub_skipped))?;
                s.nested_commands.extend(commands);
                let end = match raw_end {
                    Some(raw) if raw > result_end => raw,
                    _ => result_end,
                };
                s.word.push_str(input.slice(i, end + 1));
                index = end as isize;
            } else {
                s.word.push(c);
            }
            continue;
        }
        if c == '\'' || c == '"' {
            if !s.started && s.words.is_empty() && !s.invocation {
                s.expression = true;
            }
            quote = Some(c);
            standalone = at_boundary
                || (s.expression && !s.redirect_target)
                || (s.indexable == Indexable::Member && ends_dot_colon(&s.word));
            if !standalone {
                s.indexable = Indexable::No;
            }
            s.started = true;
            continue;
        }
        if c == '`' {
            let Some((value, end)) = powershell_escape(input, i, at_boundary || in_expression, skipped) else {
                return Err(O::UnterminatedEscape);
            };
            let raw_space = space_at(input, i + 1);
            if (in_expression || at_boundary) && s.started && raw_space {
                s.finish_word(i);
            } else if !at_boundary || !raw_space {
                // At a token boundary escaped whitespace is trivia, not a new argument.
                s.started = true;
                s.indexable = Indexable::No;
                s.word.push_str(&value);
            }
            index = end as isize;
            continue;
        }
        if !s.started
            && !s.words.is_empty()
            && !s.expression
            && s.clause.is_none()
            && !s.redirect_target
            && input.sticky(&POWERSHELL_STOP_PARSING_RE, i).is_some()
        {
            s.word = "--%".to_string();
            s.started = true;
            s.finish_word(i + 3);
            index += 2;
            continue;
        }
        if c == '<' && next == Some('#') && (at_boundary || in_expression || s.indexable == Indexable::Member) {
            let member_operator = s.indexable == Indexable::Member && ends_dot_colon(&s.previous());
            if s.started {
                s.finish_word(i);
            }
            let Some(end) = input.index_of("#>", i + 2) else {
                return Err(O::InvalidStructure);
            };
            if !member_operator {
                s.indexable = Indexable::No;
            }
            index = end as isize + 1;
            continue;
        }
        if c == '#'
            && (at_boundary
                || s.indexable == Indexable::Member
                || (in_expression && !(close == Some(']') && test(&RE_PS_TYPE_WORD, &s.word))))
        {
            let prev = s.previous();
            let member_operator = s.indexable == Indexable::Member && ends_dot_colon(&prev);
            if (s.clause.is_some() && s.clause != Some(Clause::Member)) || prev.ends_with(',') || member_operator {
                s.finish_word(i);
            } else if !s.words.is_empty() || s.started || s.invocation {
                s.finish_command(i, false);
            }
            s.statement_head = !s.dangling;
            let newline = [input.index_of_char('\n', i), input.index_of_char('\r', i)].into_iter().flatten().min();
            match newline {
                None => {
                    index = len as isize;
                    break;
                }
                Some(newline) => {
                    index = if input.is(newline, '\r') && input.is(newline + 1, '\n') { newline + 1 } else { newline }
                        as isize;
                    continue;
                }
            }
        }
        if in_expression && s.started && (c == '>' || (c == '*' && next == Some('>'))) {
            s.finish_word(i);
        }
        let redirect = if !s.started
            && (c == '>'
                || c == '*'
                || (c.is_ascii_digit()
                    && (!s.words.is_empty() || input.sticky(&POWERSHELL_MERGE_REDIRECT_RE, i).is_some())))
        {
            powershell_redirect(input, i)
        } else {
            PsRedirect::None
        };
        match redirect {
            PsRedirect::Invalid => return Err(O::InvalidRedirect),
            PsRedirect::Op(redirect) => {
                if s.redirect_target
                    || s.words.is_empty()
                    || (s.clause.is_some() && s.clause != Some(Clause::Member))
                {
                    return Err(O::InvalidRedirect);
                }
                s.redirect_target = !redirect.contains('&');
                s.indexable = Indexable::No;
                index += redirect.len() as isize - 1;
                s.command_end = index as usize + 1;
                continue;
            }
            PsRedirect::None => {}
        }
        if !s.started && s.words.is_empty() && !s.invocation && !s.expression && s.clause.is_none() && c == ':' {
            if let Some(n) = input.sticky(&POWERSHELL_LABEL_RE, i) {
                index += n as isize - 1;
                continue;
            }
        }
        if !s.started
            && !s.invocation
            && s.clause.is_none()
            && (if s.words.is_empty() { !s.expression } else { s.words.iter().all(|w| w.starts_with('[')) })
            && c.is_ascii_alphabetic()
        {
            let keyword = input.sticky(&POWERSHELL_KEYWORD_RE, i).map(|n| input.slice(i, i + n).to_lowercase());
            if let Some(keyword) = keyword.as_deref() {
                let param = || keyword == "param" && input.sticky(&POWERSHELL_PARAM_RE, i).is_some();
                let accept = if !s.words.is_empty() {
                    param() || keyword == "class" || keyword == "enum"
                } else {
                    param()
                        || (keyword == "foreach" && input.sticky(&POWERSHELL_FOREACH_RE, i).is_some())
                        || (keyword == "until" && s.after == Some(After::Do))
                        || ((keyword == "else" || keyword == "elseif") && s.after == Some(After::If))
                        || ((keyword == "catch" || keyword == "finally") && s.after == Some(After::Try))
                        || [
                            "if", "for", "while", "do", "switch", "function", "filter", "workflow", "parallel",
                            "sequence", "configuration", "try", "begin", "process", "end", "clean", "trap", "class",
                            "enum", "data", "dynamicparam", "using", "break", "continue",
                        ]
                        .contains(&keyword)
                };
                if accept {
                    s.expression = matches!(keyword, "class" | "enum" | "break" | "continue");
                    s.clause = Some(match keyword {
                        "using" | "break" | "continue" => Clause::Member,
                        "param" | "until" => Clause::Paren,
                        "while" if s.after == Some(After::Do) => Clause::Paren,
                        "do" => Clause::Do,
                        "if" | "elseif" => Clause::If,
                        "try" | "catch" => Clause::Try,
                        "switch" => Clause::Switch,
                        "class" | "enum" => Clause::Clause,
                        _ => Clause::Block,
                    });
                    s.after = None;
                    index += keyword.chars().count() as isize - 1;
                    continue;
                }
                if s.words.is_empty() && matches!(keyword, "return" | "throw" | "exit") {
                    s.after = None;
                    index += keyword.chars().count() as isize - 1;
                    continue;
                }
            }
        }
        let assign = in_expression && s.clause.is_none() && (c == '=' || ("-+*/%".contains(c) && next == Some('=')));
        if assign
            || (in_expression && close == Some(')') && !s.started && input.sticky(&POWERSHELL_IN_RE, i).is_some())
        {
            if assign {
                s.finish_word(i);
            }
            s.words.clear();
            s.raw_words.clear();
            s.word_ends.clear();
            s.expression = false;
            s.indexable = Indexable::No;
            if !assign || c != '=' {
                index += 1;
            }
            continue;
        }
        if at_boundary && c == '@' && input.sticky(&POWERSHELL_AT_OPENER_RE, i + 1).is_none() {
            return Err(O::InvalidStructure);
        }
        if at_boundary && c == '@' && (next == Some('\'') || next == Some('"')) {
            if s.started {
                s.finish_word(i);
            }
            s.word_start = i;
            let (commands, source, end) = powershell_here_string(input, i, depth, budget, skipped)?;
            if s.words.is_empty() && !s.invocation {
                s.expression = true;
            }
            s.nested_commands.extend(commands);
            s.word = source;
            s.started = true;
            index = end as isize;
            s.finish_word(end + 1);
            s.indexable = Indexable::Closed;
            continue;
        }
        if c == '$' && next == Some('{') {
            let Some(end) = powershell_braced_variable(input, i, skipped) else {
                return Err(O::InvalidStructure);
            };
            if !s.started && s.words.is_empty() && !s.invocation && hash != Hash::Switch {
                s.expression = true;
            }
            s.indexable = if at_boundary || (s.indexable == Indexable::Member && ends_dot_colon(&s.word)) {
                Indexable::Closed
            } else {
                Indexable::No
            };
            s.word.push_str(input.slice(i, end + 1));
            s.started = true;
            index = end as isize;
            if s.expression && !s.redirect_target && !input.is(end + 1, '[') {
                s.finish_word(end + 1);
            }
            continue;
        }
        let opener = if (c == '$' || (at_boundary && c == '@')) && next == Some('(') {
            Some(i + 1)
        } else if at_boundary && c == '@' && next == Some('{') {
            Some(i + 1)
        } else if c == '('
            || c == '{'
            || (c == '['
                && (in_expression
                    || s.indexable != Indexable::No
                    || (!s.started
                        && !s.invocation
                        && (s.words.is_empty()
                            || s.clause == Some(Clause::Block)
                            || s.clause == Some(Clause::Try)))))
        {
            Some(i)
        } else {
            None
        };
        if let Some(opener) = opener {
            let open_char = input.chars[opener];
            let standalone_block = at_boundary || in_expression || c == '(' || open_char == '{';
            let block_close = match open_char {
                '(' => ')',
                '[' => ']',
                _ => '}',
            };
            let block_hash = if open_char == '[' || (c == '@' && open_char == '{') {
                Hash::True
            } else if s.clause == Some(Clause::Switch) && open_char == '{' {
                Hash::Switch
            } else if (s.clause == Some(Clause::Clause) && open_char == '{')
                || (close == Some(']') && c == '(' && s.started)
            {
                Hash::Clause
            } else {
                Hash::False
            };
            if s.started && (c == '{' || c == '(' || (c == '@' && at_boundary)) {
                s.finish_word(i);
                if s.stop_parsing {
                    index -= 1;
                    continue;
                }
                s.word_start = i;
            }
            if !s.started && s.words.is_empty() && !s.invocation {
                s.expression = true;
            }
            let (commands, end) =
                scan_powershell_list(input, opener + 1, depth + 1, budget, Some(block_close), block_hash, skipped)?;
            s.nested_commands.extend(commands);
            s.started = true;
            s.word.push_str(input.slice(i, end + 1));
            index = end as isize;
            if open_char == '[' {
                s.indexable = Indexable::Closed;
                if !input.is(end + 1, '[') {
                    s.finish_word(end + 1);
                }
            } else if standalone_block {
                s.finish_word(end + 1);
                s.indexable = Indexable::Closed;
            }
            if (s.clause == Some(Clause::Paren) && c == '(')
                || (s.clause.is_some() && s.clause != Some(Clause::Paren) && c == '{')
            {
                let completed = match s.clause {
                    Some(Clause::Do) => Some(After::Do),
                    Some(Clause::If) => Some(After::If),
                    Some(Clause::Try) => Some(After::Try),
                    _ => None,
                };
                s.clause = Some(Clause::Member);
                s.finish_command(end + 1, false);
                s.after = completed;
            }
            continue;
        }
        if Some(c) == close {
            break;
        }
        if c == '}' || c == ')' || (c == ']' && in_expression) {
            return Err(O::InvalidStructure);
        }
        if !s.started
            && s.words.is_empty()
            && ((c == '&' && next != Some('&'))
                || (c == '.' && next.is_none_or(|n| js_space(n) || "\"'$(),;{|}&".contains(n))))
        {
            if s.invocation {
                return Err(O::InvalidStructure);
            }
            s.segment = i;
            s.invocation = true;
            s.after = None;
            continue;
        }
        if js_space(c) && c != '\n' && c != '\r' {
            s.finish_word(i);
            s.indexable = Indexable::No;
            continue;
        }
        let separator: Option<&str> = if c == '\r' && next == Some('\n') {
            Some("\r\n")
        } else if c == '&' && next == Some('&') {
            Some("&&")
        } else if c == '|' && next == Some('|') {
            Some("||")
        } else {
            match c {
                ';' => Some(";"),
                '|' => Some("|"),
                '&' => Some("&"),
                '\n' => Some("\n"),
                '\r' => Some("\r"),
                _ => None,
            }
        };
        if let Some(separator) = separator {
            if close == Some(']') && [";", "|", "||", "&&", "&"].contains(&separator) {
                return Err(O::InvalidStructure);
            }
            if separator == "\n" || separator == "\r" || separator == "\r\n" {
                let prev = s.previous();
                let member_operator = s.indexable == Indexable::Member && ends_dot_colon(&prev);
                if (s.clause.is_some() && s.clause != Some(Clause::Member)) || prev.ends_with(',') || member_operator {
                    s.finish_word(i);
                    index += separator.len() as isize - 1;
                    if !member_operator {
                        s.indexable = Indexable::No;
                    }
                    continue;
                }
                if !s.started && s.words.is_empty() && !s.invocation {
                    index += separator.len() as isize - 1;
                    s.indexable = Indexable::No;
                    continue;
                }
            }
            let required = s.dangling || ![";", "\n", "\r", "\r\n"].contains(&separator);
            s.finish_command(i, required);
            s.dangling = ![";", "&", "\n", "\r", "\r\n"].contains(&separator);
            s.statement_head = !s.dangling;
            index += separator.len() as isize - 1;
            continue;
        }
        if !s.started
            && s.words.is_empty()
            && !s.invocation
            && hash != Hash::Switch
            && input.sticky(&POWERSHELL_EXPRESSION_RE, i).is_some()
        {
            s.expression = true;
        }
        let next_space = next.is_some_and(js_space);
        let after_next = input.at(i + 2);
        let after_next_space = after_next.is_some_and(js_space);
        if (at_boundary || (s.indexable == Indexable::Member && ends_dot_colon(&s.word))) && c == '$' {
            s.indexable = if input.sticky(&POWERSHELL_INDEXABLE_VAR_RE, i + 1).is_some() {
                Indexable::Variable
            } else {
                Indexable::No
            };
        } else if s.indexable == Indexable::Closed {
            s.indexable = if (c == '.' && next != Some('.') && !next_space)
                || (!s.started && c == ':' && next == Some(':') && !after_next_space)
                || (!s.started && c == '?' && next == Some('.') && after_next != Some('.') && !after_next_space)
            {
                Indexable::Member
            } else if !s.started && c == '?' && next == Some('[') {
                Indexable::Closed
            } else {
                Indexable::No
            };
        } else if s.indexable == Indexable::Variable {
            s.indexable = if (c == '.' && next != Some('.') && !next_space)
                || (c == ':' && next == Some(':') && !after_next_space)
            {
                Indexable::Member
            } else if (c == '.' && next == Some('.')) || !char_test(&RE_PS_VARIABLE_CHAR, c) {
                Indexable::No
            } else {
                Indexable::Variable
            };
        } else if s.indexable == Indexable::Member {
            s.indexable = if c == '.' && next == Some('.') {
                Indexable::No
            } else if (c == '.' && !next_space)
                || (c == ':' && (next == Some(':') || input.ati(i as isize - 1) == Some(':')))
                || (c == '?' && next == Some('.') && after_next != Some('.'))
                || char_test(&RE_PS_MEMBER_CHAR, c)
            {
                Indexable::Member
            } else {
                Indexable::No
            };
        }
        s.after = None;
        s.started = true;
        s.dangling = false;
        s.word.push(c);
    }

    let end = index.max(0) as usize;
    if close.is_some() && input.at(end) != close {
        return Err(O::InvalidStructure);
    }
    if quote.is_some() {
        return Err(O::UnterminatedQuote);
    }
    s.finish_command(end, false);
    if s.redirect_target || s.invalid || s.dangling {
        return Err(O::InvalidStructure);
    }
    if s.commands.iter().any(|command| command.words.first().is_none_or(String::is_empty)) {
        return Err(O::DynamicCommandName);
    }
    Ok((s.commands, end))
}

fn powershell_escape(
    input: &Input,
    start: usize,
    line_continuation: bool,
    skipped: Option<&Skipped>,
) -> Option<(String, usize)> {
    let mut next = start + 1;
    while skip(skipped, next) {
        next += 1;
    }
    let c = input.at(next)?;
    if c == '\r' || c == '\n' {
        let crlf = line_continuation && c == '\r' && input.is(next + 1, '\n');
        return Some((if crlf { "\r\n".to_string() } else { c.to_string() }, next + usize::from(crlf)));
    }
    if c == 'u' && input.is(next + 1, '{') {
        let caps = input.sticky_caps(&POWERSHELL_UNICODE_ESCAPE_RE, next)?;
        let digits = caps[1].as_deref()?;
        let point = u32::from_str_radix(digits, 16).ok()?;
        if point > 0x10ffff {
            return None;
        }
        let whole = caps[0].as_deref()?.chars().count();
        return Some((char::from_u32(point).unwrap_or('\u{FFFD}').to_string(), next + whole - 1));
    }
    Some((ps_escape_char(c).unwrap_or(c).to_string(), next))
}

fn powershell_braced_variable(input: &Input, start: usize, skipped: Option<&Skipped>) -> Option<usize> {
    let mut index = start + 2;
    while index < input.len() {
        if skip(skipped, index) {
            index += 1;
            continue;
        }
        if input.chars[index] == '`' {
            let (_, end) = powershell_escape(input, index, false, skipped)?;
            index = end + 1;
            continue;
        }
        if input.chars[index] == '}' {
            return if index > start + 2 { Some(index) } else { None };
        }
        index += 1;
    }
    None
}

fn powershell_sub_expression(
    input: &Input,
    start: usize,
    here_string: bool,
    budget: &mut Budget,
    skipped: Option<&Skipped>,
) -> Option<(Skipped, Option<usize>)> {
    let mut next_skipped: Skipped = skipped.cloned().unwrap_or_default();
    let mut depth: i64 = 1;
    let mut raw_end: Option<usize> = None;
    let mut index = start;
    while index < input.len() {
        budget.remaining -= 1;
        if budget.remaining < 0 {
            return None;
        }
        if next_skipped.contains(&index) {
            index += 1;
            continue;
        }
        let c = input.chars[index];
        if c == '(' {
            depth += 1;
        } else if c == ')' {
            depth -= 1;
            if depth == 0 && raw_end.is_none() {
                raw_end = Some(index);
            }
        } else if !here_string && (c == '"' || c == '`') {
            let mut next = index + 1;
            while next_skipped.contains(&next) {
                next += 1;
            }
            if input.is(next, '"') {
                next_skipped.insert(index);
                index = next;
            }
        }
        index += 1;
    }
    Some((next_skipped, raw_end))
}

fn powershell_stop_parsing(input: &Input, start: usize, skipped: Option<&Skipped>) -> usize {
    let mut quoted = false;
    for index in start..input.len() {
        if skip(skipped, index) {
            continue;
        }
        let c = input.chars[index];
        if c == '"' {
            quoted = !quoted;
        }
        if c == '\r' || c == '\n' || ((c == '|' || (c == '&' && input.is(index + 1, '&'))) && !quoted) {
            return index;
        }
    }
    input.len()
}

fn powershell_here_string(
    input: &Input,
    start: usize,
    depth: usize,
    budget: &mut Budget,
    skipped: Option<&Skipped>,
) -> R<(Vec<Command>, String, usize)> {
    let Some(header) = input.sticky(&POWERSHELL_HERE_HEADER_RE, start) else {
        return Err(O::UnterminatedQuote);
    };
    let quote = input.chars[start + 1];
    let body = start + header;
    let mut commands: Vec<Command> = vec![];
    let mut line_start = true;
    let mut index = body;
    while index < input.len() {
        budget.remaining -= 1;
        if budget.remaining < 0 {
            return Err(O::InvalidStructure);
        }
        if skip(skipped, index) {
            index += 1;
            continue;
        }
        let c = input.chars[index];
        if line_start && c == quote && input.is(index + 1, '@') {
            let end = if index > body && input.is(index - 1, '\n') && input.ati(index as isize - 2) == Some('\r') {
                index - 2
            } else if index > body {
                index - 1
            } else {
                index
            };
            return Ok((commands, input.slice(body, end).to_string(), index + 1));
        }
        if quote == '"' {
            if c == '`' {
                let Some((_, end)) = powershell_escape(input, index, false, skipped) else {
                    return Err(O::UnterminatedEscape);
                };
                index = end + 1;
                line_start = false;
                continue;
            }
            if c == '$' && input.is(index + 1, '{') {
                let Some(end) = powershell_braced_variable(input, index, skipped) else {
                    return Err(O::InvalidStructure);
                };
                index = end + 1;
                line_start = false;
                continue;
            }
            if c == '$' && input.is(index + 1, '(') {
                let Some((sub_skipped, raw_end)) = powershell_sub_expression(input, index + 2, true, budget, skipped)
                else {
                    return Err(O::InvalidStructure);
                };
                let (nested, result_end) =
                    scan_powershell_list(input, index + 2, depth + 1, budget, Some(')'), Hash::False, Some(&sub_skipped))?;
                commands.extend(nested);
                index = match raw_end {
                    Some(raw) if raw > result_end => raw,
                    _ => result_end,
                } + 1;
                line_start = false;
                continue;
            }
        }
        line_start = c == '\r' || c == '\n';
        index += 1;
    }
    Err(O::UnterminatedQuote)
}

enum PsRedirect {
    None,
    Invalid,
    Op(String),
}

fn powershell_redirect(input: &Input, index: usize) -> PsRedirect {
    let mut cursor = index;
    if input.is(cursor, '*') {
        cursor += 1;
    } else {
        while input.at(cursor).is_some_and(|c| c.is_ascii_digit()) {
            cursor += 1;
        }
    }
    if !input.is(cursor, '>') {
        return PsRedirect::None;
    }
    cursor += 1;
    if input.is(cursor, '>') {
        cursor += 1;
    }
    if input.is(cursor, '&') {
        cursor += 1;
        while input.at(cursor).is_some_and(|c| c.is_ascii_digit()) {
            cursor += 1;
        }
    }
    let redirect = input.slice(index, cursor);
    if test(&RE_PS_REDIRECT, redirect) { PsRedirect::Op(redirect.to_string()) } else { PsRedirect::Invalid }
}

#[cfg(test)]
pub(crate) mod golden;

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{Value, json};

    fn utf16_to_byte(s: &str, offset: usize) -> usize {
        let mut units = 0;
        for (b, c) in s.char_indices() {
            if units >= offset {
                return b;
            }
            units += c.len_utf16();
        }
        s.len() + offset.saturating_sub(units)
    }

    fn to_json(outcome: &ScanOutcome) -> Value {
        match outcome {
            ScanOutcome::Opaque(reason) => json!({"kind": "opaque", "reason": reason.as_str()}),
            ScanOutcome::Scanned(commands) => {
                let commands: Vec<Value> = commands
                    .iter()
                    .map(|c| {
                        let mut m = serde_json::Map::new();
                        m.insert("resource".into(), c.resource.clone().into());
                        m.insert("words".into(), c.words.clone().into());
                        m.insert("rawWords".into(), c.raw_words.clone().into());
                        if let Some(ends) = &c.word_ends {
                            m.insert("wordEnds".into(), ends.clone().into());
                        }
                        if c.statement_head {
                            m.insert("statementHead".into(), true.into());
                        }
                        if c.declaration {
                            m.insert("declaration".into(), true.into());
                        }
                        if let Some(count) = c.redirect_word_count {
                            m.insert("redirectWordCount".into(), count.into());
                        }
                        Value::Object(m)
                    })
                    .collect();
                json!({"kind": "scanned", "commands": commands})
            }
        }
    }

    fn expected(mut value: Value) -> Value {
        if let Some(commands) = value.get_mut("commands").and_then(Value::as_array_mut) {
            for command in commands {
                let resource = command["resource"].as_str().unwrap_or("").to_string();
                if let Some(ends) = command.get_mut("wordEnds").and_then(Value::as_array_mut) {
                    for end in ends.iter_mut() {
                        *end = utf16_to_byte(&resource, end.as_u64().unwrap() as usize).into();
                    }
                }
            }
        }
        value
    }

    fn scanned(input: &str, dialect: Dialect) -> Vec<Command> {
        match scan(input, dialect) {
            ScanOutcome::Scanned(commands) => commands,
            other => panic!("{input:?} → {other:?}"),
        }
    }

    fn opaque(input: &str, dialect: Dialect) -> &'static str {
        match scan(input, dialect) {
            ScanOutcome::Opaque(reason) => reason.as_str(),
            other => panic!("{input:?} → {other:?}"),
        }
    }

    #[test]
    fn golden_upstream_cases() {
        let mut checked = 0;
        for line in golden::CASES.lines() {
            let case: Value = serde_json::from_str(line).unwrap();
            let got = match case["fn"].as_str().unwrap() {
                "scan" => {
                    let dialect = match case["dialect"].as_str().unwrap() {
                        "bash" => Dialect::Bash,
                        "zsh" => Dialect::Zsh,
                        _ => Dialect::Posix,
                    };
                    scan(case["input"].as_str().unwrap(), dialect)
                }
                "ps" => scan_powershell(case["input"].as_str().unwrap()),
                _ => continue,
            };
            assert_eq!(to_json(&got), expected(case["result"].clone()), "case {line}");
            checked += 1;
        }
        assert!(checked > 400);
    }

    #[test]
    fn accepts_empty_scripts_and_blank_lines() {
        for source in ["", " ", "\n\n", "# comment", "\n# comment\n\n", " \n\t# comment\n"] {
            assert_eq!(scan(source, Dialect::Posix), ScanOutcome::Scanned(vec![]), "{source:?}");
        }
    }

    #[test]
    fn ignores_blank_lines_and_comments() {
        for source in ["\n\ngit status\n\n", "# before\n\ngit status\n\n# after\n", "git status; # after\n\n"] {
            let commands = scanned(source, Dialect::Posix);
            assert_eq!(commands.len(), 1);
            assert_eq!(commands[0].resource, "git status");
            assert_eq!(commands[0].words, ["git", "status"]);
            assert_eq!(commands[0].raw_words, ["git", "status"]);
        }
    }

    #[test]
    fn retains_required_operands_across_line_breaks() {
        for operator in ["&&", "||", "|", "|&"] {
            assert!(matches!(scan(&format!("printf ok {operator}\n\n# comment\n"), Dialect::Posix), ScanOutcome::Opaque(_)));
            let commands = scanned(&format!("printf ok {operator}\n\n# comment\npwd\n\n"), Dialect::Posix);
            let resources: Vec<&str> = commands.iter().map(|c| c.resource.as_str()).collect();
            assert_eq!(resources, ["printf ok", "pwd"]);
        }
    }

    #[test]
    fn scans_lists_quotes_and_substitutions() {
        let commands = scanned("git status && curl evil | sed s/x/y/", Dialect::Posix);
        let resources: Vec<&str> = commands.iter().map(|c| c.resource.as_str()).collect();
        assert_eq!(resources, ["git status", "curl evil", "sed s/x/y/"]);

        let commands = scanned("printf '%s\\n' 'x; rm -rf /' && printf foo\\|bar", Dialect::Posix);
        assert_eq!(commands[0].words, ["printf", "%s\\n", "x; rm -rf /"]);
        assert_eq!(commands[1].resource, "printf foo\\|bar");
        assert_eq!(commands[1].words, ["printf", "foo|bar"]);

        let commands = scanned("echo \"$(curl evil | sed s/x/y/)\"", Dialect::Posix);
        let resources: Vec<&str> = commands.iter().map(|c| c.resource.as_str()).collect();
        assert_eq!(resources, ["echo \"$(curl evil | sed s/x/y/)\"", "curl evil", "sed s/x/y/"]);
        assert_eq!(commands[0].words, ["echo", "$(curl evil | sed s/x/y/)"]);

        let commands = scanned("OUT=$(printf out) X=`printf value` printenv >$(printf path)", Dialect::Posix);
        assert_eq!(commands[0].resource, "OUT=$(printf out) X=`printf value` printenv >$(printf path)");
        assert_eq!(commands[0].words, ["printenv"]);
        let rest: Vec<&str> = commands[1..].iter().map(|c| c.resource.as_str()).collect();
        assert_eq!(rest, ["printf out", "printf value", "printf path"]);
    }

    #[test]
    fn opaque_reasons() {
        assert_eq!(opaque("echo 'unterminated", Dialect::Posix), "unterminated-quote");
        assert_eq!(opaque("echo $(printf x", Dialect::Posix), "command-substitution");
        assert_eq!(opaque("cat <<EOF\nbody", Dialect::Bash), "heredoc");
        assert_eq!(opaque("printf -v'a[$(scan_probe)]' x", Dialect::Bash), "dynamic-execution");
        assert_eq!(opaque("command alias p=scan_probe\np", Dialect::Posix), "dynamic-execution");
        assert_eq!(scan(&"a".repeat(64 * 1024 + 1), Dialect::Posix), ScanOutcome::Opaque(O::InvalidStructure));
    }

    #[test]
    fn heredoc_resource_spans_the_body() {
        let commands = scanned("cat <<'EOF'\nstatic body\nEOF", Dialect::Bash);
        assert_eq!(commands.len(), 1);
        assert_eq!(commands[0].resource, "cat <<'EOF'\nstatic body\nEOF");
        assert_eq!(commands[0].words, ["cat"]);
    }

    #[test]
    fn powershell_statements_and_word_ends() {
        let ScanOutcome::Scanned(commands) = scan_powershell("Get-ChildItem -Path C:\\ ; Write-Output é ok") else {
            panic!()
        };
        assert_eq!(commands.len(), 2);
        assert_eq!(commands[0].resource, "Get-ChildItem -Path C:\\");
        assert!(commands[0].statement_head);
        assert_eq!(commands[1].words, ["Write-Output", "é", "ok"]);
        // Byte offsets into the resource.
        assert_eq!(commands[1].word_ends, Some(vec![12, 15, 18]));
        assert_eq!(scan_powershell("Write-Output \"x"), ScanOutcome::Opaque(O::UnterminatedQuote));
        assert_eq!(scan_powershell("a \u{2018}b"), ScanOutcome::Opaque(O::InvalidStructure));
    }
}
