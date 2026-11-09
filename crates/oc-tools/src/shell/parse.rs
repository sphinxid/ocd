//! Shell command scanning for permissions (`core/src/shell/parse.ts`): turns the portable scanner's
//! (`super::scan`) commands into permission resources, saved prefixes and directory changes.

use std::path::{Path, PathBuf};
use std::sync::LazyLock;

use regex::Regex;

use super::scan::{self, Dialect, ScanOutcome};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScanCommand {
    pub resource: String,
    pub save: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ScanResult {
    pub commands: Vec<ScanCommand>,
    pub directories: Vec<String>,
}

/// JavaScript `\s` as a regex character-class body.
const JS_SPACE: &str = r"\t\n\x0B\x0C\r \x{A0}\x{1680}\x{2000}-\x{200A}\x{2028}\x{2029}\x{202F}\x{205F}\x{3000}\x{FEFF}";

const CWD: &[&str] = &["cd", "chdir", "popd", "pushd", "push-location", "set-location"];

static POWERSHELL_PATH_FLAG_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)^-(?:p(?:ath|at|a)?|lp|l(?:i(?:t(?:e(?:r(?:a(?:l(?:p(?:a(?:t(?:h)?)?)?)?)?)?)?)?)?)?|psp(?:a(?:t(?:h)?)?)?):?$")
        .unwrap()
});
static POWERSHELL_LITERAL_FLAG_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)^-(?:lp|l(?:i(?:t(?:e(?:r(?:a(?:l(?:p(?:a(?:t(?:h)?)?)?)?)?)?)?)?)?)?|psp(?:a(?:t(?:h)?)?)?):?$").unwrap()
});
static POWERSHELL_STACK_FLAG_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)^-(?:stack(?:name|nam|na|n)?|st):?$").unwrap());

const ARITY: &[(&str, usize)] = &[
    ("cat", 1),
    ("cd", 1),
    ("chmod", 1),
    ("chown", 1),
    ("cp", 1),
    ("echo", 1),
    ("env", 1),
    ("export", 1),
    ("grep", 1),
    ("kill", 1),
    ("killall", 1),
    ("ln", 1),
    ("ls", 1),
    ("mkdir", 1),
    ("mv", 1),
    ("ps", 1),
    ("pwd", 1),
    ("rm", 1),
    ("rmdir", 1),
    ("sleep", 1),
    ("source", 1),
    ("tail", 1),
    ("touch", 1),
    ("unset", 1),
    ("which", 1),
    ("aws", 3),
    ("az", 3),
    ("bazel", 2),
    ("brew", 2),
    ("bun", 2),
    ("bun run", 3),
    ("bun x", 3),
    ("cargo", 2),
    ("cargo add", 3),
    ("cargo run", 3),
    ("cdk", 2),
    ("cf", 2),
    ("cmake", 2),
    ("composer", 2),
    ("consul", 2),
    ("consul kv", 3),
    ("crictl", 2),
    ("deno", 2),
    ("deno task", 3),
    ("doctl", 3),
    ("docker", 2),
    ("docker builder", 3),
    ("docker compose", 3),
    ("docker container", 3),
    ("docker image", 3),
    ("docker network", 3),
    ("docker volume", 3),
    ("eksctl", 2),
    ("eksctl create", 3),
    ("firebase", 2),
    ("flyctl", 2),
    ("gcloud", 3),
    ("gh", 3),
    ("git", 2),
    ("git config", 3),
    ("git remote", 3),
    ("git stash", 3),
    ("go", 2),
    ("gradle", 2),
    ("helm", 2),
    ("heroku", 2),
    ("hugo", 2),
    ("ip", 2),
    ("ip addr", 3),
    ("ip link", 3),
    ("ip netns", 3),
    ("ip route", 3),
    ("kind", 2),
    ("kind create", 3),
    ("kubectl", 2),
    ("kubectl kustomize", 3),
    ("kubectl rollout", 3),
    ("kustomize", 2),
    ("make", 2),
    ("mc", 2),
    ("mc admin", 3),
    ("minikube", 2),
    ("mongosh", 2),
    ("mysql", 2),
    ("mvn", 2),
    ("ng", 2),
    ("npm", 2),
    ("npm exec", 3),
    ("npm init", 3),
    ("npm run", 3),
    ("npm view", 3),
    ("npx", 2),
    ("nvm", 2),
    ("nx", 2),
    ("openssl", 2),
    ("openssl req", 3),
    ("openssl x509", 3),
    ("pip", 2),
    ("pipenv", 2),
    ("pnpm", 2),
    ("pnpm dlx", 3),
    ("pnpm exec", 3),
    ("pnpm run", 3),
    ("poetry", 2),
    ("podman", 2),
    ("podman container", 3),
    ("podman image", 3),
    ("psql", 2),
    ("pulumi", 2),
    ("pulumi stack", 3),
    ("python", 2),
    ("pyenv", 2),
    ("rake", 2),
    ("rbenv", 2),
    ("redis-cli", 2),
    ("rustup", 2),
    ("serverless", 2),
    ("sfdx", 3),
    ("skaffold", 2),
    ("sls", 2),
    ("sst", 2),
    ("swift", 2),
    ("systemctl", 2),
    ("terraform", 2),
    ("terraform workspace", 3),
    ("tmux", 2),
    ("turbo", 2),
    ("ufw", 2),
    ("vault", 2),
    ("vault auth", 3),
    ("vault kv", 3),
    ("vercel", 2),
    ("volta", 2),
    ("wp", 2),
    ("yarn", 2),
    ("yarn dlx", 3),
    ("yarn run", 3),
];
/// `Math.max(...Object.values(ARITY))`
const PREFIX_LENGTH: usize = 3;

fn arity(key: &str) -> Option<usize> {
    ARITY.iter().find(|(name, _)| *name == key).map(|(_, value)| *value)
}

/// Longest leading token sequence present in `ARITY` → its first `arity` tokens; else the first token.
pub fn prefix(tokens: &[String]) -> Vec<String> {
    for length in (1..=tokens.len()).rev() {
        if let Some(value) = arity(&tokens[..length].join(" ")) {
            return tokens[..value.min(tokens.len())].to_vec();
        }
    }
    tokens.iter().take(1).cloned().collect()
}

/// `ShellSelect.name`
pub fn shell_name(shell: &str) -> String {
    if cfg!(windows) {
        let normalized = windows_path(shell).replace('\\', "/");
        let base = normalized.rsplit('/').next().unwrap_or("");
        let stem = match base.rfind('.') {
            Some(dot) if dot > 0 => &base[..dot],
            _ => base,
        };
        return stem.to_lowercase();
    }
    let trimmed = shell.trim_end_matches('/');
    trimmed.rsplit('/').next().unwrap_or("").to_lowercase()
}

/// `ShellSelect.ps`
pub fn is_powershell(shell: &str) -> bool {
    matches!(shell_name(shell).as_str(), "powershell" | "pwsh")
}

fn windows_path(p: &str) -> String {
    static DRIVE_COLON: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^/([a-zA-Z]):(?:[\\/]|$)").unwrap());
    static DRIVE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^/([a-zA-Z])(?:/|$)").unwrap());
    static CYGDRIVE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^/cygdrive/([a-zA-Z])(?:/|$)").unwrap());
    static MNT: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^/mnt/([a-zA-Z])(?:/|$)").unwrap());
    let mut out = p.to_string();
    for re in [&*DRIVE_COLON, &*DRIVE, &*CYGDRIVE, &*MNT] {
        out = re.replace(&out, |c: &regex::Captures| format!("{}:/", c[1].to_uppercase())).into_owned();
    }
    out
}

/// Mirrors `ShellParse.scan`: the portable scanner's commands, with permission resources, saved prefixes and the
/// directories that directory commands change to. `shell` = full shell path; `home` = `os.homedir()`.
/// `Err` is the model-visible message.
pub fn scan(command: &str, shell: &str, cwd: &str, home: &Path) -> Result<ScanResult, String> {
    let powershell = is_powershell(shell);
    let name = shell_name(shell);
    let outcome = if powershell {
        scan::scan_powershell(command)
    } else {
        let dialect = match name.as_str() {
            "bash" => Dialect::Bash,
            "zsh" => Dialect::Zsh,
            _ => Dialect::Posix,
        };
        scan::scan(command, dialect)
    };
    let commands = match outcome {
        ScanOutcome::Opaque(reason) => {
            return Err(format!("Portable shell scanner cannot analyze command: {}", reason.as_str()));
        }
        ScanOutcome::Scanned(commands) => commands,
    };

    let mut output = ScanResult::default();
    for item in commands {
        // Declarations are skipped as standalone permission resources, while nested substitutions remain.
        if item.declaration {
            continue;
        }
        if let Some(prefix_end) = item.redirect_prefix_end {
            let head: String = item.resource.chars().take(prefix_end).collect();
            let separator =
                item.resource.chars().nth(prefix_end).filter(|next| matches!(next, '\t' | '\n')).unwrap_or(' ');
            output.commands.push(ScanCommand { save: format!("{head}{separator}*"), resource: item.resource });
            continue;
        }
        let words: Vec<String> = match item.redirect_word_count {
            None => item.raw_words.clone(),
            Some(count) => item.raw_words.iter().take(count).cloned().collect(),
        };
        // Bare statement-head foreach blocks act as PowerShell control flow rather than cmdlet invocations.
        if powershell
            && item.statement_head
            && words.first().is_some_and(|w| is_foreach_head(w))
            && words.len() > 1
            && words[1..].iter().all(|word| word.starts_with('{'))
        {
            continue;
        }
        let parts = if powershell { powershell_parts(&words, &item.words) } else { bash_parts(&item) };
        let unwrapped = unwrap_directory_command(if powershell { &words } else { &item.raw_words }, powershell);
        if let Some(offset) = unwrapped {
            let command = &parts[offset.min(parts.len())..];
            let (directories, complete) = directory_args(command, powershell, cwd, shell, home);
            output.directories.extend(directories);
            // Wrapped directory commands (`builtin cd`, `command cd`) stay permission resources.
            if offset == 0 && complete {
                continue;
            }
        }
        let head: Vec<String> = words.iter().take(PREFIX_LENGTH).cloned().collect();
        let selected = prefix(&head);
        let conventional = format!("{} *", selected.join(" "));
        let end = if selected.is_empty() {
            None
        } else {
            item.word_ends.as_ref().and_then(|ends| ends.get(selected.len() - 1)).copied()
        };
        // Keep existing grants stable unless normalized spacing loses the original source boundary.
        let save = match end {
            Some(end) if powershell && !wildcard_match(&item.resource, &conventional) => {
                let resource = &item.resource;
                if resource.get(end..).and_then(|rest| rest.chars().next()) == Some('(') {
                    format!("{}(*", &resource[..end])
                } else {
                    let boundary = item
                        .word_ends
                        .as_ref()
                        .and_then(|ends| {
                            ends.iter().copied().find(|&value| {
                                value >= end
                                    && (value >= resource.len()
                                        || resource
                                            .get(value..)
                                            .and_then(|rest| rest.chars().next())
                                            .is_some_and(scan::js_space))
                            })
                        })
                        .unwrap_or(end)
                        .min(resource.len());
                    static SEPARATOR: LazyLock<Regex> =
                        LazyLock::new(|| Regex::new(&format!("^[{JS_SPACE}]+(?:`(?:\\r\\n|\\r|\\n)[{JS_SPACE}]*)?")).unwrap());
                    let rest = resource.get(boundary..).unwrap_or("");
                    let separator = SEPARATOR.find(rest).map(|m| m.as_str()).unwrap_or(" ");
                    format!("{}{}*", resource.get(..boundary).unwrap_or(resource), separator)
                }
            }
            _ => conventional,
        };
        output.commands.push(ScanCommand { resource: item.resource, save });
    }
    Ok(output)
}

/// A command word as the directory analysis sees it: `command_parameter` for PowerShell `-Name` tokens, else `word`.
#[derive(Clone)]
struct Part {
    parameter: bool,
    text: String,
    raw: String,
    unquoted_tilde: bool,
}

static RE_POWERSHELL_PARAMETER: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^-[A-Za-z_?][A-Za-z0-9_?-]*(?::|$)").unwrap());
static RE_QUOTE_OR_BACKTICK: LazyLock<Regex> = LazyLock::new(|| Regex::new(r#"['"`]"#).unwrap());

/// `String.prototype.slice(0, end)` where `end` may be `-1` (`indexOf` miss): drops the last UTF-16 unit.
fn js_slice_to(text: &str, end: Option<usize>) -> String {
    match end {
        Some(end) => text[..end].to_string(),
        None => {
            let mut chars = text.chars();
            chars.next_back();
            chars.as_str().to_string()
        }
    }
}

fn powershell_parts(words: &[String], cooked: &[String]) -> Vec<Part> {
    let mut parts = vec![];
    for (index, raw) in words.iter().enumerate() {
        let text = if !raw.contains("`$") && RE_QUOTE_OR_BACKTICK.is_match(raw) {
            cooked.get(index).cloned().unwrap_or_else(|| raw.clone())
        } else {
            raw.replace("`$", "\0")
        };
        let part = |parameter: bool, text: String, raw: String| Part { parameter, text, raw, unquoted_tilde: false };
        if !RE_POWERSHELL_PARAMETER.is_match(raw) {
            parts.push(part(false, text, raw.clone()));
            continue;
        }
        match text.find(':') {
            Some(colon) => {
                let raw_colon = raw.find(':');
                if colon + 1 < text.len() {
                    parts.push(part(true, text[..colon].to_string(), js_slice_to(raw, raw_colon)));
                    let raw_value = match raw_colon {
                        Some(raw_colon) => raw[raw_colon + 1..].to_string(),
                        None => raw.clone(),
                    };
                    parts.push(part(false, text[colon + 1..].to_string(), raw_value));
                } else {
                    parts.push(part(true, text[..colon].to_string(), raw.clone()));
                }
            }
            None => parts.push(part(true, text, raw.clone())),
        }
    }
    parts
}

fn bash_parts(item: &scan::Command) -> Vec<Part> {
    item.raw_words
        .iter()
        .enumerate()
        .map(|(index, raw)| {
            let word = item.words.get(index).unwrap_or(raw);
            Part {
                parameter: false,
                text: if is_static_bash_word(raw) { word.replace('$', "\0") } else { "$".to_string() },
                raw: raw.clone(),
                unquoted_tilde: raw.starts_with('~'),
            }
        })
        .collect()
}

fn is_foreach_head(word: &str) -> bool {
    let lower = word.to_lowercase();
    lower == "foreach" || lower.starts_with("foreach-")
}

/// The offset of the directory command within `raw_words`: 0 for `cd …`, past `builtin`/`command` wrappers
/// (and their options) for Bash. `command -v cd` only looks the command up.
fn unwrap_directory_command(raw_words: &[String], powershell: bool) -> Option<usize> {
    let head = raw_words.first().map(|w| if powershell { w.to_lowercase() } else { w.clone() });
    if CWD.contains(&head.as_deref().unwrap_or("")) {
        return Some(0);
    }
    if powershell {
        return None;
    }
    static LOOKUP_FLAG: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^-[p]*[vV]").unwrap());
    let mut index = 0;
    while let Some(wrapper @ ("builtin" | "command")) = raw_words.get(index).map(String::as_str) {
        index += 1;
        while let Some(flag) = raw_words.get(index).filter(|w| w.starts_with('-')) {
            index += 1;
            if flag == "--" {
                break;
            }
            if wrapper == "command" && LOOKUP_FLAG.is_match(flag) {
                return None;
            }
        }
    }
    (index > 0 && CWD.contains(&raw_words.get(index).map(String::as_str).unwrap_or(""))).then_some(index)
}

/// Whether a Bash word expands to exactly its cooked text: no expansions, brace lists or ranges, and no globs
/// in a dot-leading path segment (`.?`, `.[.]` can match `..`).
fn is_static_bash_word(raw: &str) -> bool {
    #[derive(PartialEq)]
    enum Quote {
        Single,
        Double,
        Ansi,
    }
    struct Track {
        segment_started: bool,
        segment_leading_dot: bool,
        segment_glob: bool,
        has_ansi_escape: bool,
        has_unquoted_glob: bool,
    }
    impl Track {
        fn record(&mut self, c: char, unquoted_glob: bool) -> bool {
            if unquoted_glob {
                self.has_unquoted_glob = true;
            }
            if self.has_ansi_escape && self.has_unquoted_glob {
                return false;
            }
            if c == '/' {
                self.segment_started = false;
                self.segment_leading_dot = false;
                self.segment_glob = false;
                return true;
            }
            if !self.segment_started {
                self.segment_started = true;
                self.segment_leading_dot = c == '.';
            }
            if unquoted_glob {
                self.segment_glob = true;
            }
            !(self.segment_leading_dot && self.segment_glob)
        }
    }
    let dynamic_next = |c: Option<char>| c.is_some_and(|c| c.is_ascii_alphanumeric() || "_@*#?$!({[-".contains(c));
    let chars: Vec<char> = raw.chars().collect();
    let mut quote: Option<Quote> = None;
    let mut track = Track {
        segment_started: false,
        segment_leading_dot: false,
        segment_glob: false,
        has_ansi_escape: false,
        has_unquoted_glob: false,
    };
    let mut braces: Vec<bool> = vec![];
    let mut index = 0;
    while index < chars.len() {
        let c = chars[index];
        let next = chars.get(index + 1).copied();
        let continuation = next == Some('\\') && chars.get(index + 2) == Some(&'\n');
        match quote {
            Some(Quote::Single) => {
                if c == '\'' {
                    quote = None;
                } else if !track.record(c, false) {
                    return false;
                }
            }
            Some(Quote::Ansi) => {
                if c == '\\' {
                    track.has_ansi_escape = true;
                    if track.has_unquoted_glob {
                        return false;
                    }
                    index += 1;
                } else if c == '\'' {
                    quote = None;
                } else if !track.record(c, false) {
                    return false;
                }
            }
            Some(Quote::Double) => {
                if c == '\\' && next.is_some_and(|n| "$`\"\\\n".contains(n)) {
                    index += 1;
                    if next != Some('\n') && !track.record(next.unwrap(), false) {
                        return false;
                    }
                } else if c == '"' {
                    quote = None;
                } else if c == '`' || (c == '$' && (dynamic_next(next) || continuation)) || !track.record(c, false) {
                    return false;
                }
            }
            None => {
                if c == '\\' {
                    index += 1;
                    // A trailing backslash records nothing (`raw[index + 1] ?? ""`).
                    if let Some(next) = next
                        && next != '\n'
                        && !track.record(next, false)
                    {
                        return false;
                    }
                } else if c == '\'' {
                    quote = Some(Quote::Single);
                } else if c == '"' {
                    quote = Some(Quote::Double);
                } else if c == '$' && next == Some('\'') {
                    quote = Some(Quote::Ansi);
                    index += 1;
                } else if c == '$' && next == Some('"') {
                    quote = Some(Quote::Double);
                    index += 1;
                } else if c == '`'
                    || ((c == '<' || c == '>' || c == '=') && next == Some('('))
                    || (c == '$' && (dynamic_next(next) || continuation))
                {
                    return false;
                } else if c == '{' {
                    braces.push(false);
                    if !track.record(c, false) {
                        return false;
                    }
                } else {
                    if !braces.is_empty() && (c == ',' || (c == '.' && next == Some('.'))) {
                        *braces.last_mut().unwrap() = true;
                    }
                    if c == '}' && !braces.is_empty() {
                        if braces.pop() == Some(true) || !track.record(c, false) {
                            return false;
                        }
                    } else if !track.record(c, c == '*' || c == '?' || c == '[') {
                        return false;
                    }
                }
            }
        }
        index += 1;
    }
    quote.is_none()
}

/// Whether a PowerShell path has a wildcard in a dot-leading segment, which can match `..`.
fn has_powershell_dot_wildcard(raw: &str) -> bool {
    struct Track {
        segment_started: bool,
        segment_leading_dot: bool,
        segment_glob: bool,
        has_quote_escape: bool,
        has_unquoted_glob: bool,
    }
    impl Track {
        fn record(&mut self, c: Option<char>, unquoted_glob: bool) -> bool {
            if unquoted_glob {
                self.has_unquoted_glob = true;
            }
            if self.has_quote_escape && self.has_unquoted_glob {
                return true;
            }
            if c == Some('/') || c == Some('\\') {
                self.segment_started = false;
                self.segment_leading_dot = false;
                self.segment_glob = false;
                return false;
            }
            if !self.segment_started {
                self.segment_started = true;
                self.segment_leading_dot = c == Some('.');
            }
            if unquoted_glob {
                self.segment_glob = true;
            }
            self.segment_leading_dot && self.segment_glob
        }
    }
    let chars: Vec<char> = raw.chars().collect();
    let mut quote: Option<char> = None;
    let mut track = Track {
        segment_started: false,
        segment_leading_dot: false,
        segment_glob: false,
        has_quote_escape: false,
        has_unquoted_glob: false,
    };
    let mut index = 0;
    while index < chars.len() {
        let c = chars[index];
        let next = chars.get(index + 1).copied();
        match quote {
            Some('\'') => {
                if c == '\'' && next == Some('\'') {
                    index += 1;
                    if track.record(Some('\''), false) {
                        return true;
                    }
                } else if c == '\'' {
                    quote = None;
                } else if track.record(Some(c), false) {
                    return true;
                }
            }
            Some(_) => {
                if c == '`' {
                    track.has_quote_escape = true;
                    if track.has_unquoted_glob {
                        return true;
                    }
                    index += 1;
                } else if c == '"' && next == Some('"') {
                    index += 1;
                    if track.record(Some('"'), false) {
                        return true;
                    }
                } else if c == '"' {
                    quote = None;
                } else if track.record(Some(c), false) {
                    return true;
                }
            }
            None => {
                if c == '`' {
                    index += 1;
                    // `raw[index + 1] ?? ""` records an empty segment start after a trailing backtick.
                    if track.record(next, false) {
                        return true;
                    }
                } else if c == '\'' || c == '"' {
                    quote = Some(c);
                } else if track.record(Some(c), c == '*' || c == '?' || c == '[') {
                    return true;
                }
            }
        }
        index += 1;
    }
    false
}

/// `Wildcard.match` (`core/src/util/wildcard.ts`).
fn wildcard_match(input: &str, pattern: &str) -> bool {
    let normalized = input.replace('\\', "/");
    let mut escaped = String::new();
    for c in pattern.replace('\\', "/").chars() {
        match c {
            '*' => escaped.push_str(".*"),
            '?' => escaped.push('.'),
            '.' | '+' | '^' | '$' | '{' | '}' | '(' | ')' | '|' | '[' | ']' | '\\' => {
                escaped.push('\\');
                escaped.push(c);
            }
            _ => escaped.push(c),
        }
    }
    if let Some(stripped) = escaped.strip_suffix(" .*") {
        escaped = format!("{stripped}( .*)?");
    }
    let flags = if cfg!(windows) { "(?si)" } else { "(?s)" };
    Regex::new(&format!("{flags}^{escaped}$")).is_ok_and(|re| re.is_match(&normalized))
}

/// The directories a directory command changes to, and whether they are all it can change to (a dynamic or
/// unresolvable operand, `cd -`, `popd`, or a missing operand leaves the command as a permission resource).
fn directory_args(command: &[Part], powershell: bool, cwd: &str, shell: &str, home: &Path) -> (Vec<String>, bool) {
    let head = command.first().map(|part| part.text.to_lowercase()).unwrap_or_default();
    if head == "popd" || head == "pop-location" {
        return (vec![], false);
    }

    if !powershell {
        let mut end_of_options = false;
        let operands: Vec<&Part> = command
            .iter()
            .skip(1)
            .filter(|part| {
                if end_of_options {
                    return true;
                }
                if part.text == "--" {
                    end_of_options = true;
                    return false;
                }
                part.text == "-" || !part.text.starts_with('-')
            })
            .collect();
        if operands.is_empty() {
            return (vec![], false);
        }
        let directories: Vec<String> = operands
            .iter()
            .filter(|part| part.text != "-")
            .filter_map(|part| directory_argument(&part.text, false, cwd, shell, home, part.unquoted_tilde))
            .collect();
        let complete = directories.len() == operands.len();
        return (directories, complete);
    }

    let mut directories = vec![];
    let mut path_flag = false;
    let mut literal_flag = false;
    let mut end_of_parameters = false;
    let mut saw_operand = false;
    let mut complete = true;
    for part in command.iter().skip(1) {
        if !end_of_parameters && part.text == "--" {
            end_of_parameters = true;
            continue;
        }
        if path_flag {
            saw_operand = true;
            let value = if !literal_flag
                && (has_powershell_dot_wildcard(&part.raw) || part.text == "-" || part.text == "+")
            {
                None
            } else {
                directory_argument(&part.text, true, cwd, shell, home, false)
            };
            match value {
                Some(value) => directories.push(value),
                None => complete = false,
            }
            path_flag = false;
            literal_flag = false;
            continue;
        }
        if !end_of_parameters && part.parameter {
            if POWERSHELL_STACK_FLAG_RE.is_match(&part.text) {
                return (directories, false);
            }
            path_flag = POWERSHELL_PATH_FLAG_RE.is_match(&part.text);
            literal_flag = POWERSHELL_LITERAL_FLAG_RE.is_match(&part.text);
            continue;
        }
        saw_operand = true;
        let stack_shortcut = !end_of_parameters && (part.raw == "-" || part.raw == "+");
        let value = if has_powershell_dot_wildcard(&part.raw) || stack_shortcut {
            None
        } else {
            directory_argument(&part.text, true, cwd, shell, home, false)
        };
        match value {
            Some(value) => directories.push(value),
            None => complete = false,
        }
    }
    (directories, saw_operand && complete)
}

fn directory_argument(
    value: &str,
    powershell: bool,
    cwd: &str,
    shell: &str,
    home: &Path,
    unquoted_tilde: bool,
) -> Option<String> {
    if !powershell {
        return expand_known_directory(value, false, cwd, home, unquoted_tilde);
    }
    let quote = value.chars().next();
    let text = match quote {
        Some(q @ ('"' | '\'')) if value.ends_with(q) => {
            // `value.slice(1, -1)`; a lone quote character yields "".
            let inner: Vec<char> = value.chars().collect();
            if inner.len() >= 2 { inner[1..inner.len() - 1].iter().collect() } else { String::new() }
        }
        _ => value.to_string(),
    };
    // PowerShell exposes environment variables through $env:NAME and provides these
    // automatic directory variables. Expand only values we can determine without executing code.
    static BRACED_ENV: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?i)\$\{env:([^}]+)\}").unwrap());
    static ENV: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?i)\$env:([A-Za-z_][A-Za-z0-9_]*)").unwrap());
    static AUTOMATIC: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"(?i)\$(?:\{(HOME|PWD|PSHOME)\}|(HOME|PWD|PSHOME))").unwrap());
    let text = BRACED_ENV.replace_all(&text, |c: &regex::Captures| environment(&c[1]).unwrap_or_default());
    let text = ENV.replace_all(&text, |c: &regex::Captures| environment(&c[1]).unwrap_or_default());
    // `(?=$|[\\/])` after the bare form is checked by hand (the regex crate has no lookahead).
    let mut expanded = String::new();
    let mut last_end = 0;
    for captures in AUTOMATIC.captures_iter(&text) {
        let whole = captures.get(0).unwrap();
        if captures.get(2).is_some() {
            let after = &text[whole.end()..];
            if !(after.is_empty() || after.starts_with('/') || after.starts_with('\\')) {
                continue;
            }
        }
        let key = captures.get(1).or(captures.get(2)).map(|m| m.as_str().to_uppercase()).unwrap_or_default();
        expanded.push_str(&text[last_end..whole.start()]);
        expanded.push_str(&match key.as_str() {
            "HOME" => home.to_string_lossy().into_owned(),
            "PWD" => cwd.to_string(),
            _ => dirname(shell),
        });
        last_end = whole.end();
    }
    expanded.push_str(&text[last_end..]);
    expand_known_directory(&expanded, true, cwd, home, false)
}

fn dirname(path: &str) -> String {
    match Path::new(path).parent() {
        Some(parent) if parent.as_os_str().is_empty() => ".".to_string(),
        Some(parent) => parent.to_string_lossy().into_owned(),
        None => path.to_string(),
    }
}

fn expand_known_directory(
    value: &str,
    powershell: bool,
    cwd: &str,
    home: &Path,
    unquoted_tilde: bool,
) -> Option<String> {
    // Unknown shell expressions cannot be resolved safely during permission analysis.
    if value.contains('$') || value.contains('`') || (powershell && value.starts_with('(')) {
        return None;
    }
    let resolved = value.replace('\0', "$");
    let home_text = home.to_string_lossy();
    if resolved == "~" {
        return Some(home_text.into_owned());
    }
    if resolved.starts_with("~/") || (cfg!(windows) && resolved.starts_with("~\\")) {
        return Some(join(&home_text, &resolved[2..]));
    }
    if unquoted_tilde && !cfg!(windows) {
        if resolved == "~+" {
            return Some(cwd.to_string());
        }
        if let Some(rest) = resolved.strip_prefix("~+/") {
            return Some(join(cwd, rest));
        }
        // `~-`, `~+N`, `~-N` and `~N` name directory-stack entries.
        static STACK: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^~(?:[-+][0-9]*|[0-9]+)").unwrap());
        static USER: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^~([A-Za-z_][A-Za-z0-9._-]*)").unwrap());
        let bounded = |end: usize| resolved[end..].is_empty() || resolved[end..].starts_with('/');
        if STACK.find(&resolved).is_some_and(|m| bounded(m.end())) {
            return None;
        }
        if let Some(user) = USER.captures(&resolved).and_then(|c| c.get(1)).filter(|m| bounded(m.end())) {
            let user_home = resolve_user_home(user.as_str(), &home_text)?;
            return Some(join(&user_home, &resolved[user.end()..]));
        }
    }
    Some(resolved)
}

/// `/etc/passwd` login names to home directories (later lines win, like `new Map(entries)`).
fn passwd_homes() -> &'static std::collections::HashMap<String, String> {
    static HOMES: std::sync::OnceLock<std::collections::HashMap<String, String>> = std::sync::OnceLock::new();
    HOMES.get_or_init(|| {
        let content = std::fs::read_to_string("/etc/passwd").unwrap_or_default();
        content
            .split('\n')
            .filter(|line| !line.is_empty() && !line.starts_with('#'))
            .filter_map(|line| {
                let fields: Vec<&str> = line.split(':').collect();
                let name = fields.first().copied().unwrap_or("");
                let home = fields.get(5).copied().unwrap_or("");
                (!name.is_empty() && !home.is_empty()).then(|| (name.to_string(), home.to_string()))
            })
            .collect()
    })
}

/// `os.userInfo()` under Bun: the effective user's login name, with `os.homedir()` as the home.
fn current_username() -> Option<String> {
    #[cfg(unix)]
    {
        let mut buffer = vec![0 as libc::c_char; 16 * 1024];
        let mut entry: libc::passwd = unsafe { std::mem::zeroed() };
        let mut found: *mut libc::passwd = std::ptr::null_mut();
        // SAFETY: getpwuid_r writes into `entry` and `buffer`, which outlive the call and the reads below.
        let status =
            unsafe { libc::getpwuid_r(libc::geteuid(), &mut entry, buffer.as_mut_ptr(), buffer.len(), &mut found) };
        if status != 0 || found.is_null() || entry.pw_name.is_null() {
            return None;
        }
        // SAFETY: pw_name points into `buffer` as a NUL-terminated string.
        let name = unsafe { std::ffi::CStr::from_ptr(entry.pw_name) };
        Some(name.to_string_lossy().into_owned())
    }
    #[cfg(not(unix))]
    {
        None
    }
}

fn resolve_user_home(user: &str, home: &str) -> Option<String> {
    if !home.is_empty() && current_username().as_deref() == Some(user) {
        return Some(home.to_string());
    }
    if let Some(home) = passwd_homes().get(user) {
        return Some(home.clone());
    }
    if cfg!(target_os = "macos") {
        let candidate = format!("/Users/{user}");
        if Path::new(&candidate).is_dir() {
            return Some(candidate);
        }
    }
    None
}

/// `path.join(base, rest)`: joins and normalizes `.`/`..` segments and duplicate separators.
fn join(base: &str, rest: &str) -> String {
    if rest.is_empty() {
        return normalize(base);
    }
    normalize(&format!("{base}/{rest}"))
}

fn normalize(path: &str) -> String {
    let absolute = path.starts_with('/');
    let trailing = path.ends_with('/') && path.len() > 1;
    let mut parts: Vec<&str> = vec![];
    for segment in path.split('/') {
        match segment {
            "" | "." => {}
            ".." => {
                if parts.last().is_some_and(|last| *last != "..") {
                    parts.pop();
                } else if !absolute {
                    parts.push("..");
                }
            }
            other => parts.push(other),
        }
    }
    let mut out = parts.join("/");
    if absolute {
        out.insert(0, '/');
    }
    if trailing && !out.ends_with('/') {
        out.push('/');
    }
    if out.is_empty() {
        out.push('.');
    }
    PathBuf::from(out).to_string_lossy().into_owned()
}

fn environment(key: &str) -> Option<String> {
    if !cfg!(windows) {
        return std::env::var(key).ok();
    }
    std::env::vars().find(|(name, _)| name.eq_ignore_ascii_case(key)).map(|(_, value)| value)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{Value, json};

    fn run(command: &str, shell: &str) -> Result<ScanResult, String> {
        scan(command, shell, "/workspace", Path::new("/home/user"))
    }

    fn cmd(resource: &str, save: &str) -> ScanCommand {
        ScanCommand { resource: resource.into(), save: save.into() }
    }

    fn to_json(result: Result<ScanResult, String>) -> Value {
        match result {
            Ok(result) => json!({
                "commands": result.commands.iter().map(|c| json!({"resource": c.resource, "save": c.save})).collect::<Vec<_>>(),
                "directories": result.directories,
            }),
            Err(error) => json!({"error": error}),
        }
    }

    /// None for other functions' cases, else whether the port reproduces the recorded result.
    fn check_parse_case(line: &str) -> Option<bool> {
        let case: Value = serde_json::from_str(line).unwrap();
        if case["fn"] != "parse" {
            return None;
        }
        let result = scan(
            case["command"].as_str().unwrap(),
            case["shell"].as_str().unwrap(),
            case["cwd"].as_str().unwrap(),
            Path::new(case["home"].as_str().unwrap_or("/home/user")),
        );
        let expected = match case.get("error") {
            Some(error) => json!({"error": error}),
            None => case["result"].clone(),
        };
        let got = to_json(result);
        if got != expected && std::env::var("OCD_SHELL_ORACLE_VERBOSE").is_ok() {
            eprintln!("GOT {got}
FOR {line}");
        }
        Some(got == expected)
    }

    // Recorded from upstream on Linux: on Windows `parse.ts` resolves paths with `path.win32` and leaves Bash's tilde
    // prefixes (`~+`, `~-`, `~user`) alone, so these POSIX cases do not apply there.
    #[cfg(unix)]
    #[test]
    fn golden_upstream_cases() {
        let mut checked = 0;
        for line in super::super::scan::golden::CASES.lines() {
            if let Some(matched) = check_parse_case(line) {
                assert!(matched, "case {line}");
                checked += 1;
            }
        }
        assert!(checked > 300, "{checked}");
    }

    /// Every `ShellParse.scan` call recorded from upstream's test suites (`OCD_SHELL_ORACLE` names the JSON lines
    /// file; see `scan::golden`). Cases that read `/etc/passwd` must be recorded on this machine; PowerShell
    /// `$env:` cases read the recording process's environment and are skipped.
    #[test]
    fn full_upstream_oracle() {
        let Ok(path) = std::env::var("OCD_SHELL_ORACLE") else {
            return;
        };
        let content = std::fs::read_to_string(path).unwrap();
        let environment = Regex::new(r"(?i)\$\{?env:").unwrap();
        let results: Vec<(&str, bool)> = content
            .lines()
            .filter(|line| !environment.is_match(line))
            .filter_map(|line| check_parse_case(line).map(|matched| (line, matched)))
            .collect();
        let failed: Vec<&str> = results.iter().filter(|(_, matched)| !matched).map(|(line, _)| *line).collect();
        eprintln!("parse oracle cases: {} checked, {} differ", results.len(), failed.len());
        for line in failed.iter().take(20) {
            eprintln!("DIFF {line}");
        }
        assert!(failed.is_empty());
    }

    #[test]
    fn lists_and_arity_prefixes() {
        assert_eq!(run("git status && npm run test -- --watch", "bash").unwrap(), ScanResult {
            commands: vec![cmd("git status", "git status *"), cmd("npm run test -- --watch", "npm run test *")],
            directories: vec![],
        });
    }

    #[test]
    fn directories_are_separated_from_commands() {
        assert_eq!(run("cd 'src dir' && git status", "bash").unwrap(), ScanResult {
            commands: vec![cmd("git status", "git status *")],
            directories: vec!["src dir".into()],
        });
        assert_eq!(run("cd ~/x", "/bin/bash").unwrap().directories, ["/home/user/x"]);
        assert_eq!(run("cd ~/", "/bin/bash").unwrap().directories, ["/home/user"]);
        // A dynamic `cd` keeps the command as a permission resource.
        let dynamic = run("cd $HOME", "bash").unwrap();
        assert!(dynamic.directories.is_empty());
        assert_eq!(dynamic.commands, vec![cmd("cd $HOME", "cd *")]);
    }

    #[test]
    fn wrapped_and_tilde_directories() {
        let wrapped = run("command cd /outside; builtin cd /elsewhere; command -v cd", "bash").unwrap();
        assert_eq!(wrapped.directories, ["/outside", "/elsewhere"]);
        assert_eq!(wrapped.commands, vec![
            cmd("command cd /outside", "command *"),
            cmd("builtin cd /elsewhere", "builtin *"),
            cmd("command -v cd", "command *"),
        ]);
        let tilde = run("cd ~+/../outside && cd ~- && cd ~+1 && cd ~0 && cd ~__opencode_nonexistent_user__", "/bin/bash")
            .unwrap();
        if cfg!(windows) {
            // Windows has no Bash tilde prefixes (`expandKnownDirectory`): they stay as written, as plain directories.
            assert_eq!(tilde.directories, ["~+/../outside", "~-", "~+1", "~0", "~__opencode_nonexistent_user__"]);
            assert!(tilde.commands.is_empty());
        } else {
            assert_eq!(tilde.directories, ["/outside"]);
            assert_eq!(tilde.commands.len(), 4);
        }
        let globs = run("cd {..,x} && cd .[.] && cd .? && cd $'\\x2e'[.] && cd '.[.]'", "/bin/bash").unwrap();
        assert_eq!(globs.directories, [".[.]"]);
        assert_eq!(globs.commands.len(), 4);
    }

    #[test]
    fn heredoc_resource_is_the_whole_text() {
        let command = "cat <<'EOF'\nstatic body\nEOF";
        assert_eq!(run(command, "bash").unwrap().commands, vec![cmd(command, "cat *")]);
    }

    #[test]
    fn declarations_produce_only_nested_commands() {
        assert_eq!(run("export A=$(git rev-parse HEAD)", "bash").unwrap().commands, vec![cmd(
            "git rev-parse HEAD",
            "git rev-parse *"
        )]);
    }

    #[test]
    fn redirects_and_method_calls_are_resources() {
        assert_eq!(run(">> ~/.zshrc <<< 'x'; export -p > ~/.bashrc", "/bin/zsh").unwrap().commands, vec![
            cmd(">> ~/.zshrc <<< 'x'", ">> *"),
            cmd("export -p > ~/.bashrc", "export *"),
        ]);
        assert_eq!(
            run("[IO.File]::Delete(\"C:\\victim\"); $proc.Kill(); \"x\" >> ~/.bashrc", "pwsh").unwrap().commands,
            vec![
                cmd("[IO.File]::Delete(\"C:\\victim\")", "[IO.File]::Delete(*"),
                cmd("$proc.Kill()", "$proc.Kill(*"),
                cmd("\"x\" >> ~/.bashrc", "\"x\" *"),
            ]
        );
    }

    #[test]
    fn opaque_is_an_error() {
        assert_eq!(run("cat <<EOF\nbody", "bash").unwrap_err(), "Portable shell scanner cannot analyze command: heredoc");
    }

    #[test]
    fn powershell_directories_and_case() {
        let result = run("Set-Location -Path $HOME/src; Get-ChildItem", "pwsh").unwrap();
        assert_eq!(result.directories, ["/home/user/src"]);
        assert_eq!(result.commands, vec![cmd("Get-ChildItem", "Get-ChildItem *")]);
        let literal = run("Set-Location -LiteralPath '..\\outside'; Get-ChildItem", "pwsh").unwrap();
        assert_eq!(literal.directories, ["..\\outside"]);
        let stack = run("Push-Location -StackName s /outside", "pwsh").unwrap();
        assert!(stack.directories.is_empty());
        assert_eq!(stack.commands, vec![cmd("Push-Location -StackName s /outside", "Push-Location *")]);
    }

    #[test]
    fn prefix_uses_longest_known_arity() {
        let tokens = |s: &str| s.split(' ').map(str::to_string).collect::<Vec<_>>();
        assert_eq!(prefix(&tokens("git config user.name x")), tokens("git config user.name"));
        assert_eq!(prefix(&tokens("docker compose up -d")), tokens("docker compose up"));
        assert_eq!(prefix(&tokens("cat a b")), tokens("cat"));
        assert_eq!(prefix(&tokens("unknown a b")), tokens("unknown"));
        assert_eq!(prefix(&tokens("aws s3")), tokens("aws s3"));
        assert!(prefix(&[]).is_empty());
    }

    #[test]
    fn shell_names() {
        assert_eq!(shell_name("/usr/bin/Bash"), "bash");
        assert!(is_powershell("/opt/microsoft/powershell/7/pwsh"));
        assert!(!is_powershell("/bin/zsh"));
    }
}
