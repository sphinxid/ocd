//! Shell command scanning for permissions (`core/src/shell/parse.ts`).
//!
//! The legacy scanner walks a tree-sitter parse (tree-sitter-bash 0.25.0, tree-sitter-powershell 0.25.10, the
//! grammar versions upstream ships as wasm). The portable scanner (`super::scan`) is a hand-written equivalent.

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

/// Mirrors `ShellParse.scan`. `shell` = full shell path; `home` = `os.homedir()`.
/// `Err` is the model-visible message.
pub fn scan(command: &str, shell: &str, cwd: &str, home: &Path, portable: bool) -> Result<ScanResult, String> {
    if portable {
        return scan_portable(command, shell, cwd, home);
    }
    scan_legacy(command, shell, cwd, home)
}

#[derive(Clone)]
struct Part {
    kind: String,
    text: String,
}

fn scan_legacy(command: &str, shell: &str, cwd: &str, home: &Path) -> Result<ScanResult, String> {
    let powershell = is_powershell(shell);
    let mut parser = tree_sitter::Parser::new();
    let language: tree_sitter::Language = if powershell {
        tree_sitter_powershell::LANGUAGE.into()
    } else {
        tree_sitter_bash::LANGUAGE.into()
    };
    parser.set_language(&language).map_err(|_| "Failed to parse shell command".to_string())?;
    let tree = parser.parse(command, None).ok_or_else(|| "Failed to parse shell command".to_string())?;
    let source = command.as_bytes();
    let mut result = ScanResult::default();
    for node in descendants_of_type(tree.root_node(), "command") {
        let command_parts = parts(node, source);
        let tokens: Vec<String> = command_parts.iter().map(|part| part.text.clone()).collect();
        if tokens.is_empty() {
            continue;
        }
        let name = if powershell { tokens[0].to_lowercase() } else { tokens[0].clone() };
        if CWD.contains(&name.as_str()) {
            result.directories.extend(directory_args(&command_parts, powershell, cwd, shell, home));
            continue;
        }
        let target = match node.parent() {
            Some(parent) if parent.kind() == "redirected_statement" => parent,
            _ => node,
        };
        result.commands.push(ScanCommand {
            resource: scan::js_trim(node_text(target, source)).to_string(),
            save: format!("{} *", prefix(&tokens).join(" ")),
        });
    }
    Ok(result)
}

fn node_text<'a>(node: tree_sitter::Node, source: &'a [u8]) -> &'a str {
    node.utf8_text(source).unwrap_or("")
}

/// `Node.descendantsOfType` (web-tree-sitter): a preorder cursor walk from the node itself, skipping subtrees
/// that end at or before the zero start point.
fn descendants_of_type<'t>(root: tree_sitter::Node<'t>, kind: &str) -> Vec<tree_sitter::Node<'t>> {
    let mut result = vec![];
    let mut cursor = root.walk();
    let mut visited_children = false;
    let zero = tree_sitter::Point { row: 0, column: 0 };
    loop {
        let node = cursor.node();
        if !visited_children {
            if node.end_position() <= zero {
                if cursor.goto_next_sibling() {
                    visited_children = false;
                } else {
                    if !cursor.goto_parent() {
                        break;
                    }
                    visited_children = true;
                }
                continue;
            }
            if node.kind() == kind {
                result.push(node);
            }
            if cursor.goto_first_child() || cursor.goto_next_sibling() {
                visited_children = false;
            } else {
                if !cursor.goto_parent() {
                    break;
                }
                visited_children = true;
            }
        } else if cursor.goto_next_sibling() {
            visited_children = false;
        } else if !cursor.goto_parent() {
            break;
        }
    }
    result
}

fn parts(node: tree_sitter::Node, source: &[u8]) -> Vec<Part> {
    let mut result = vec![];
    for index in 0..node.child_count() {
        let Some(child) = node.child(index) else {
            continue;
        };
        if child.kind() == "command_elements" {
            for item_index in 0..child.child_count() {
                let Some(item) = child.child(item_index) else {
                    continue;
                };
                if item.kind() == "command_argument_sep" || item.kind() == "redirection" {
                    continue;
                }
                result.push(Part { kind: item.kind().to_string(), text: node_text(item, source).to_string() });
            }
            continue;
        }
        if !["command_name", "command_name_expr", "word", "string", "raw_string", "concatenation"]
            .contains(&child.kind())
        {
            continue;
        }
        result.push(Part { kind: child.kind().to_string(), text: node_text(child, source).to_string() });
    }
    result
}

static RE_POWERSHELL_PARAMETER: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^-[A-Za-z_?][A-Za-z0-9_?-]*(?::|$)").unwrap());
static RE_QUOTE_OR_BACKTICK: LazyLock<Regex> = LazyLock::new(|| Regex::new(r#"['"`]"#).unwrap());
static RE_BASH_DYNAMIC: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"[$`~\\]").unwrap());
static RE_QUOTES: LazyLock<Regex> = LazyLock::new(|| Regex::new(r#"['"]"#).unwrap());

fn scan_portable(command: &str, shell: &str, cwd: &str, home: &Path) -> Result<ScanResult, String> {
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
        // The legacy command walk skips declarations, not the substitutions within them.
        if item.declaration {
            continue;
        }
        let words: Vec<String> = match item.redirect_word_count {
            None => item.raw_words.clone(),
            Some(count) => item.raw_words.iter().take(count).cloned().collect(),
        };
        // The shipped PowerShell grammar treats bare statement-head foreach blocks as control flow.
        if powershell
            && item.statement_head
            && words.first().is_some_and(|w| is_foreach_head(w))
            && words.len() > 1
            && words[1..].iter().all(|word| word.starts_with('{'))
        {
            continue;
        }
        let first = words.first().map(|w| if powershell { w.to_lowercase() } else { w.clone() });
        if first.as_deref().is_some_and(|name| CWD.contains(&name)) {
            let parts: Vec<Part> = if powershell {
                words
                    .iter()
                    .enumerate()
                    .flat_map(|(index, raw)| {
                        let text = if !raw.contains("`$") && RE_QUOTE_OR_BACKTICK.is_match(raw) {
                            item.words.get(index).cloned().unwrap_or_else(|| raw.clone())
                        } else {
                            raw.replace("`$", "\0")
                        };
                        if !RE_POWERSHELL_PARAMETER.is_match(raw) {
                            return vec![Part { kind: "word".into(), text }];
                        }
                        match text.find(':') {
                            Some(colon) if colon + 1 < text.len() => vec![
                                Part { kind: "command_parameter".into(), text: text[..colon].to_string() },
                                Part { kind: "word".into(), text: text[colon + 1..].to_string() },
                            ],
                            Some(colon) => {
                                vec![Part { kind: "command_parameter".into(), text: text[..colon].to_string() }]
                            }
                            None => vec![Part { kind: "command_parameter".into(), text }],
                        }
                    })
                    .collect()
            } else {
                item.raw_words
                    .iter()
                    .enumerate()
                    .map(|(index, text)| Part {
                        kind: "word".into(),
                        // Only literal quoting resolves to a static directory.
                        text: if text.starts_with("$'")
                            || (!RE_BASH_DYNAMIC.is_match(text) && RE_QUOTES.is_match(text))
                        {
                            item.words.get(index).cloned().unwrap_or_default()
                        } else {
                            text.clone()
                        },
                    })
                    .collect()
            };
            output.directories.extend(directory_args(&parts, powershell, cwd, shell, home));
            continue;
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
                let boundary = item
                    .word_ends
                    .as_ref()
                    .and_then(|ends| {
                        ends.iter().copied().find(|&value| {
                            value >= end
                                && (value >= resource.len()
                                    || resource.get(value..).and_then(|rest| rest.chars().next()).is_some_and(scan::js_space))
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
            _ => conventional,
        };
        output.commands.push(ScanCommand { resource: item.resource, save });
    }
    Ok(output)
}

fn is_foreach_head(word: &str) -> bool {
    let lower = word.to_lowercase();
    lower == "foreach" || lower.starts_with("foreach-")
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

fn directory_args(command: &[Part], powershell: bool, cwd: &str, shell: &str, home: &Path) -> Vec<String> {
    if !powershell {
        return command
            .iter()
            .skip(1)
            .filter(|part| !part.text.starts_with('-'))
            .filter_map(|part| directory_argument(&part.text, powershell, cwd, shell, home))
            .collect();
    }
    let mut directories = vec![];
    let mut path = false;
    let mut end_of_parameters = false;
    for part in command.iter().skip(1) {
        if !end_of_parameters && part.text == "--" {
            end_of_parameters = true;
            continue;
        }
        if path {
            if let Some(value) = directory_argument(&part.text, powershell, cwd, shell, home) {
                directories.push(value);
            }
            path = false;
            continue;
        }
        if !end_of_parameters && part.kind == "command_parameter" {
            path = POWERSHELL_PATH_FLAG_RE.is_match(&part.text);
            continue;
        }
        if let Some(value) = directory_argument(&part.text, powershell, cwd, shell, home) {
            directories.push(value);
        }
    }
    directories
}

fn directory_argument(value: &str, powershell: bool, cwd: &str, shell: &str, home: &Path) -> Option<String> {
    let mut chars = value.chars();
    let quote = chars.next();
    let last = value.chars().last();
    let text = match quote {
        Some(q @ ('"' | '\'')) if last == Some(q) => {
            // `value.slice(1, -1)`; a lone quote character yields "".
            let inner: Vec<char> = value.chars().collect();
            if inner.len() >= 2 { inner[1..inner.len() - 1].iter().collect() } else { String::new() }
        }
        _ => value.to_string(),
    };
    if !powershell {
        return expand_known_directory(&text, home);
    }
    // PowerShell exposes environment variables through $env:NAME and provides these
    // automatic directory variables. Expand only values we can determine without executing code.
    static BRACED_ENV: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?i)\$\{env:([^}]+)\}").unwrap());
    static ENV: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?i)\$env:([A-Za-z_][A-Za-z0-9_]*)").unwrap());
    static AUTOMATIC: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(r"(?i)\$(?:\{(HOME|PWD|PSHOME)\}|(HOME|PWD|PSHOME))").unwrap()
    });
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
    expand_known_directory(&expanded, home)
}

fn dirname(path: &str) -> String {
    match Path::new(path).parent() {
        Some(parent) if parent.as_os_str().is_empty() => ".".to_string(),
        Some(parent) => parent.to_string_lossy().into_owned(),
        None => path.to_string(),
    }
}

fn expand_known_directory(value: &str, home: &Path) -> Option<String> {
    // Unknown shell expressions cannot be resolved safely during permission analysis.
    if value.contains('$') || value.contains('`') || value.starts_with('(') {
        return None;
    }
    let resolved = value.replace('\0', "$");
    if resolved == "~" {
        return Some(home.to_string_lossy().into_owned());
    }
    if resolved.starts_with("~/") || (cfg!(windows) && resolved.starts_with("~\\")) {
        return Some(join(home, &resolved[2..]));
    }
    Some(resolved)
}

/// `path.join(home, rest)`: joins and normalizes `.`/`..` segments and duplicate separators.
fn join(home: &Path, rest: &str) -> String {
    let joined = format!("{}/{}", home.to_string_lossy(), rest);
    normalize(&joined)
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

    fn run(command: &str, shell: &str, portable: bool) -> Result<ScanResult, String> {
        scan(command, shell, "/workspace", Path::new("/home/user"), portable)
    }

    fn cmd(resource: &str, save: &str) -> ScanCommand {
        ScanCommand { resource: resource.into(), save: save.into() }
    }

    #[test]
    fn golden_upstream_cases() {
        let mut checked = 0;
        for line in super::super::scan::golden::CASES.lines() {
            let case: Value = serde_json::from_str(line).unwrap();
            if case["fn"] != "parse" {
                continue;
            }
            let result = scan(
                case["command"].as_str().unwrap(),
                case["shell"].as_str().unwrap(),
                case["cwd"].as_str().unwrap(),
                Path::new("/home/user"),
                case["portable"].as_bool().unwrap(),
            );
            let got = match result {
                Ok(result) => json!({
                    "commands": result.commands.iter().map(|c| json!({"resource": c.resource, "save": c.save})).collect::<Vec<_>>(),
                    "directories": result.directories,
                }),
                Err(error) => json!({"error": error}),
            };
            let expected = match case.get("error") {
                Some(error) => json!({"error": error}),
                None => case["result"].clone(),
            };
            assert_eq!(got, expected, "case {line}");
            checked += 1;
        }
        assert!(checked > 300);
    }

    #[test]
    fn lists_and_arity_prefixes() {
        for portable in [false, true] {
            assert_eq!(
                run("git status && npm run test -- --watch", "bash", portable).unwrap(),
                ScanResult {
                    commands: vec![cmd("git status", "git status *"), cmd("npm run test -- --watch", "npm run test *")],
                    directories: vec![],
                }
            );
        }
    }

    #[test]
    fn directories_are_separated_from_commands() {
        for portable in [false, true] {
            assert_eq!(
                run("cd 'src dir' && git status", "bash", portable).unwrap(),
                ScanResult { commands: vec![cmd("git status", "git status *")], directories: vec!["src dir".into()] }
            );
            assert_eq!(run("cd ~/x", "/bin/bash", portable).unwrap().directories, ["/home/user/x"]);
            assert!(run("cd $HOME", "bash", portable).unwrap().directories.is_empty());
        }
    }

    #[test]
    fn heredoc_resource_is_the_whole_text() {
        let command = "cat <<'EOF'\nstatic body\nEOF";
        assert_eq!(run(command, "bash", false).unwrap().commands, vec![cmd(command, "cat *")]);
    }

    #[test]
    fn legacy_declarations_produce_only_nested_commands() {
        assert_eq!(run("export A=$(git rev-parse HEAD)", "bash", false).unwrap().commands, vec![cmd(
            "git rev-parse HEAD",
            "git rev-parse *"
        )]);
    }

    #[test]
    fn portable_opaque_is_an_error() {
        assert_eq!(
            run("cat <<EOF\nbody", "bash", true).unwrap_err(),
            "Portable shell scanner cannot analyze command: heredoc"
        );
    }

    #[test]
    fn powershell_directories_and_case() {
        // Upstream parity: the legacy grammar splits `$HOME/src` into separate elements.
        for (portable, directory) in [(false, "/home/user"), (true, "/home/user/src")] {
            let result = run("Set-Location -Path $HOME/src; Get-ChildItem", "pwsh", portable).unwrap();
            assert_eq!(result.directories, [directory]);
            assert_eq!(result.commands, vec![cmd("Get-ChildItem", "Get-ChildItem *")]);
        }
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
