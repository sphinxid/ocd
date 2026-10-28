//! Small ports of shared upstream helpers used by mini: `util/path-format.ts`, `util/tool-display.ts`,
//! `util/filetype.ts`, `prompt/parse.ts`, `prompt/display.ts`, `ui/select-controller.ts`.

use serde_json::{Map, Value};

/// `os.homedir()`.
pub fn home_dir() -> String {
    std::env::var("HOME").ok().filter(|h| !h.is_empty()).unwrap_or_else(|| dirs::home_dir().map(|p| p.to_string_lossy().into_owned()).unwrap_or_default())
}

fn windows_path(input: &str) -> bool {
    let b = input.as_bytes();
    (b.len() >= 3 && b[0].is_ascii_alphabetic() && b[1] == b':' && (b[2] == b'\\' || b[2] == b'/')) || input.starts_with("\\\\")
}

/// `path.posix.normalize` of an absolute or relative path (no trailing slash except root).
fn normalize(path: &str) -> String {
    let absolute = path.starts_with('/');
    let mut parts: Vec<&str> = Vec::new();
    for seg in path.split('/') {
        match seg {
            "" | "." => {}
            ".." => {
                if parts.last().is_some_and(|p| *p != "..") {
                    parts.pop();
                } else if !absolute {
                    parts.push("..");
                }
            }
            s => parts.push(s),
        }
    }
    let joined = parts.join("/");
    if absolute { format!("/{joined}") } else if joined.is_empty() { ".".into() } else { joined }
}

/// `path.posix.resolve(base, input)`.
pub fn resolve(base: &str, input: &str) -> String {
    if input.starts_with('/') { normalize(input) } else { normalize(&format!("{base}/{input}")) }
}

/// `path.posix.relative(from, to)` for absolute paths.
pub fn relative(from: &str, to: &str) -> String {
    let from = normalize(from);
    let to = normalize(to);
    if from == to {
        return String::new();
    }
    let f: Vec<&str> = from.split('/').filter(|s| !s.is_empty()).collect();
    let t: Vec<&str> = to.split('/').filter(|s| !s.is_empty()).collect();
    let common = f.iter().zip(t.iter()).take_while(|(a, b)| a == b).count();
    let mut out: Vec<&str> = vec![".."; f.len() - common];
    out.extend(&t[common..]);
    out.join("/")
}

/// `abbreviateHome(input, home)`.
pub fn abbreviate_home(input: &str, home: &str) -> String {
    if home.is_empty() {
        return input.to_string();
    }
    let rel = relative(home, input);
    if rel.is_empty() {
        return "~".into();
    }
    if rel == ".." || rel.starts_with("../") || rel.starts_with('/') {
        return input.to_string();
    }
    format!("~/{rel}")
}

/// `formatPath(input, {base, home, forwardSlashes: true})` (posix bases).
pub fn format_path(input: Option<&str>, base: &str, home: Option<&str>) -> String {
    let Some(input) = input.filter(|s| !s.is_empty()) else { return String::new() };
    if !windows_path(base) && windows_path(input) {
        return input.replace('\\', "/");
    }
    let absolute = if input.starts_with('/') { input.to_string() } else { resolve(base, input) };
    let rel = relative(base, &absolute);
    let formatted = if rel.is_empty() {
        ".".to_string()
    } else if rel != ".." && !rel.starts_with("../") && !rel.starts_with('/') {
        rel
    } else if let Some(home) = home {
        abbreviate_home(&absolute, home)
    } else {
        absolute
    };
    formatted.replace('\\', "/")
}

pub fn cwd() -> String {
    std::env::current_dir().map(|p| p.to_string_lossy().into_owned()).unwrap_or_else(|_| "/".into())
}

/// `canonicalToolName`.
pub fn canonical_tool_name(name: &str) -> &str {
    match name {
        "bash" => "shell",
        "task" => "subagent",
        "apply_patch" => "patch",
        n => n,
    }
}

/// `finiteNumber`.
pub fn finite_number(v: Option<&Value>) -> Option<f64> {
    v.and_then(|v| v.as_f64()).filter(|n| n.is_finite())
}

/// JS `String(value)` for primitives.
pub fn js_string(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Bool(b) => b.to_string(),
        Value::Number(n) => match n.as_f64() {
            Some(f) if n.is_f64() => crate::util::locale::js_number(f),
            _ => n.to_string(),
        },
        Value::Null => "null".into(),
        other => other.to_string(),
    }
}

/// `primitiveInputSummary(input, omit)`: `[key=value, ...]`.
pub fn primitive_input_summary(input: &Map<String, Value>, omit: &[&str]) -> String {
    let entries: Vec<String> = input
        .iter()
        .filter(|(k, v)| !omit.contains(&k.as_str()) && matches!(v, Value::String(_) | Value::Number(_) | Value::Bool(_)))
        .map(|(k, v)| format!("{k}={}", js_string(v)))
        .collect();
    if entries.is_empty() { String::new() } else { format!("[{}]", entries.join(", ")) }
}

/// `webSearchProviderName`.
pub fn web_search_provider_name(provider: Option<&Value>) -> String {
    let Some(p) = provider.and_then(|p| p.as_str()).filter(|p| !p.is_empty()) else { return String::new() };
    if p == "opencode" {
        return "OpenCode".into();
    }
    let mut c = p.chars();
    let first = c.next().unwrap();
    format!("{}{}", first.to_uppercase(), c.as_str())
}

/// `webSearchProviderLabel`.
pub fn web_search_provider_label(provider: Option<&Value>) -> String {
    let name = web_search_provider_name(provider);
    if name.is_empty() { "Web Search".into() } else { format!("Web Search via {name}") }
}

/// `dict(v)`.
pub fn dict(v: Option<&Value>) -> Map<String, Value> {
    match v {
        Some(Value::Object(m)) => m.clone(),
        _ => Map::new(),
    }
}

/// `text(v)`.
pub fn text(v: Option<&Value>) -> String {
    v.and_then(|v| v.as_str()).unwrap_or_default().to_string()
}

/// `path.extname`.
pub fn extname(path: &str) -> &str {
    let base = path.rsplit('/').next().unwrap_or(path);
    match base.rfind('.') {
        Some(0) | None => "",
        Some(i) => &base[i..],
    }
}

/// `LANGUAGE_EXTENSIONS[ext]` (`util/filetype.ts`).
pub fn language_extension(ext: &str) -> Option<&'static str> {
    Some(match ext {
        ".abap" => "abap",
        ".bat" => "bat",
        ".bib" => "bibtex",
        ".bibtex" => "bibtex",
        ".clj" => "clojure",
        ".cljs" => "clojure",
        ".cljc" => "clojure",
        ".edn" => "clojure",
        ".coffee" => "coffeescript",
        ".c" => "c",
        ".cpp" => "cpp",
        ".cxx" => "cpp",
        ".cc" => "cpp",
        ".c++" => "cpp",
        ".cppm" => "cpp",
        ".ccm" => "cpp",
        ".cxxm" => "cpp",
        ".c++m" => "cpp",
        ".ixx" => "cpp",
        ".cs" => "csharp",
        ".csx" => "csharp",
        ".css" => "css",
        ".d" => "d",
        ".pas" => "pascal",
        ".pascal" => "pascal",
        ".diff" => "diff",
        ".patch" => "diff",
        ".dart" => "dart",
        ".dockerfile" => "dockerfile",
        ".ex" => "elixir",
        ".exs" => "elixir",
        ".erl" => "erlang",
        ".ets" => "typescript",
        ".hrl" => "erlang",
        ".fs" => "fsharp",
        ".fsi" => "fsharp",
        ".fsx" => "fsharp",
        ".fsscript" => "fsharp",
        ".gitcommit" => "git-commit",
        ".gitrebase" => "git-rebase",
        ".go" => "go",
        ".groovy" => "groovy",
        ".gleam" => "gleam",
        ".hbs" => "handlebars",
        ".handlebars" => "handlebars",
        ".hs" => "haskell",
        ".html" => "html",
        ".htm" => "html",
        ".ini" => "ini",
        ".java" => "java",
        ".jl" => "julia",
        ".js" => "javascript",
        ".kt" => "kotlin",
        ".kts" => "kotlin",
        ".jsx" => "javascriptreact",
        ".json" => "json",
        ".jsonc" => "jsonc",
        ".tex" => "latex",
        ".latex" => "latex",
        ".less" => "less",
        ".lua" => "lua",
        ".makefile" => "makefile",
        "makefile" => "makefile",
        ".md" => "markdown",
        ".markdown" => "markdown",
        ".m" => "objective-c",
        ".mm" => "objective-cpp",
        ".pl" => "perl",
        ".pm" => "perl",
        ".pm6" => "perl6",
        ".php" => "php",
        ".ps1" => "powershell",
        ".psm1" => "powershell",
        ".pug" => "jade",
        ".jade" => "jade",
        ".py" => "python",
        ".r" => "r",
        ".cshtml" => "razor",
        ".razor" => "razor",
        ".rb" => "ruby",
        ".rake" => "ruby",
        ".gemspec" => "ruby",
        ".ru" => "ruby",
        ".erb" => "erb",
        ".html.erb" => "erb",
        ".js.erb" => "erb",
        ".css.erb" => "erb",
        ".json.erb" => "erb",
        ".rs" => "rust",
        ".scss" => "scss",
        ".sass" => "sass",
        ".scala" => "scala",
        ".shader" => "shaderlab",
        ".sh" => "bash",
        ".bash" => "bash",
        ".zsh" => "bash",
        ".ksh" => "bash",
        ".sql" => "sql",
        ".svelte" => "svelte",
        ".swift" => "swift",
        ".ts" => "typescript",
        ".tsx" => "typescriptreact",
        ".mts" => "typescript",
        ".cts" => "typescript",
        ".mtsx" => "typescriptreact",
        ".ctsx" => "typescriptreact",
        ".xml" => "xml",
        ".xsl" => "xsl",
        ".yaml" => "yaml",
        ".yml" => "yaml",
        ".mjs" => "javascript",
        ".cjs" => "javascript",
        ".vue" => "vue",
        ".zig" => "zig",
        ".zon" => "zig",
        ".astro" => "astro",
        ".ml" => "ocaml",
        ".mli" => "ocaml",
        ".tf" => "terraform",
        ".tfvars" => "terraform-vars",
        ".hcl" => "hcl",
        ".nix" => "nix",
        ".typ" => "typst",
        ".typc" => "typst",
        _ => return None,
    })
}

/// `parseFileLineRange`.
pub fn parse_file_line_range(input: &str) -> (String, Option<(u64, Option<u64>)>) {
    let Some(hash) = input.rfind('#') else { return (input.to_string(), None) };
    let base = input[..hash].to_string();
    let rest = &input[hash + 1..];
    let re = regex::Regex::new(r"^(\d+)(?:-(\d*))?$").unwrap();
    let Some(m) = re.captures(rest) else { return (base, None) };
    let start: u64 = m[1].parse().unwrap_or(0);
    let end = m.get(2).map(|e| e.as_str()).filter(|e| !e.is_empty()).and_then(|e| e.parse::<u64>().ok()).filter(|e| start < *e);
    (base, Some((start, end)))
}

pub fn strip_file_line_range(input: &str) -> String {
    parse_file_line_range(input).0
}

/// `parseSlashHead(text)`: `{name, arguments, end}` with `end` a UTF-16 index.
pub struct SlashHead {
    pub name: String,
    pub arguments: String,
    /// Byte index in the text where the name ends.
    pub end: usize,
}

pub fn parse_slash_head(text: &str) -> Option<SlashHead> {
    let rest = text.strip_prefix('/')?;
    match rest.find([' ', '\t', '\n']) {
        None => Some(SlashHead { name: rest.to_string(), arguments: String::new(), end: text.len() }),
        Some(i) => {
            let split = i + 1;
            let args_start = split + rest[i..].chars().next().map(|c| c.len_utf8()).unwrap_or(1);
            Some(SlashHead { name: text[1..split].to_string(), arguments: text.get(args_start..).unwrap_or("").to_string(), end: split })
        }
    }
}

fn js_ws(c: char) -> bool {
    c.is_whitespace() || c == '\u{feff}'
}

/// `mentionTriggerIndex(text)` (offset in edit units = chars here).
pub fn mention_trigger_index(text: &str) -> Option<usize> {
    let index = text.rfind('@')?;
    let before = text[..index].chars().last();
    let query = &text[index..];
    if (before.is_none() || before.is_some_and(js_ws)) && !query.chars().any(js_ws) {
        return Some(crate::mini::prompt_shared::measure_edit_text(&text[..index]));
    }
    None
}

/// `slashTriggerIndex(text)`.
pub fn slash_trigger_index(text: &str) -> Option<usize> {
    let mut search = text.len();
    while let Some(index) = text[..search].rfind('/') {
        let before = text[..index].chars().last();
        let query = &text[index..];
        if before.is_some_and(|c| !js_ws(c)) {
            if index == 0 {
                break;
            }
            search = index;
            continue;
        }
        if query.chars().any(js_ws) || query[1..].contains('/') {
            return None;
        }
        return Some(crate::mini::prompt_shared::measure_edit_text(&text[..index]));
    }
    None
}

/// `reconcileSelection`.
pub fn reconcile_selection(selected: i64, count: i64) -> i64 {
    (count - 1).min(selected).max(0)
}

/// `moveSelection` with the clamp policy.
pub fn move_selection_clamp(selected: i64, count: i64, delta: i64) -> i64 {
    if count <= 0 {
        return 0;
    }
    reconcile_selection(selected + delta, count)
}

fn max_offset(count: i64, limit: i64) -> i64 {
    (count - limit).max(0)
}

/// `revealSelectionOffset`.
pub fn reveal_selection_offset(offset: i64, count: i64, limit: i64, selected: i64) -> i64 {
    let max = max_offset(count, limit);
    if selected < offset {
        return max.min(selected);
    }
    if selected >= offset + limit {
        return max.min(selected - limit + 1);
    }
    offset.min(max).max(0)
}

/// `moveSelectionOffset`.
pub fn move_selection_offset(offset: i64, count: i64, limit: i64, selected: i64, direction: i64) -> i64 {
    let max = max_offset(count, limit);
    let margin = ((limit - 1) / 2).clamp(0, 2);
    if direction < 0 && selected < offset + margin {
        return (selected - margin).min(max).max(0);
    }
    if direction > 0 && selected > offset + limit - margin - 1 {
        return max.min(selected - limit + margin + 1);
    }
    offset.min(max).max(0)
}

/// `Intl.NumberFormat("en-US", {style: "currency", currency: "USD"})`.
pub fn money(v: f64) -> String {
    let neg = v < 0.0;
    let cents = (v.abs() * 100.0).round() as i64;
    let whole = cents / 100;
    let frac = cents % 100;
    let mut digits = whole.to_string();
    let mut grouped = String::new();
    while digits.len() > 3 {
        let tail = digits.split_off(digits.len() - 3);
        grouped = format!(",{tail}{grouped}");
    }
    format!("{}${digits}{grouped}.{frac:02}", if neg { "-" } else { "" })
}

/// Drop leading graphemes covering `width` display cells.
pub fn skip_width(s: &str, width: usize) -> String {
    use unicode_segmentation::UnicodeSegmentation;
    let mut used = 0;
    let mut out = String::new();
    for g in s.graphemes(true) {
        if used >= width {
            out.push_str(g);
        } else {
            used += crate::util::locale::width(g);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn paths() {
        assert_eq!(format_path(Some("..cache/result.txt"), "/work/project", None), "..cache/result.txt");
        assert_eq!(format_path(Some("../shared/result.txt"), "/work/project", None), "/work/shared/result.txt");
        assert_eq!(format_path(Some("/work/project"), "/work/project", None), ".");
        assert_eq!(format_path(Some("/home/u/x/y"), "/home/u/p", Some("/home/u")), "~/x/y");
        assert_eq!(format_path(Some("src/a.ts"), "/w", None), "src/a.ts");
    }

    #[test]
    fn slash_heads() {
        let h = parse_slash_head("/review now please").unwrap();
        assert_eq!((h.name.as_str(), h.arguments.as_str(), h.end), ("review", "now please", 7));
        let h = parse_slash_head("/new").unwrap();
        assert_eq!((h.name.as_str(), h.arguments.as_str(), h.end), ("new", "", 4));
        assert!(parse_slash_head("x").is_none());
    }

    #[test]
    fn triggers() {
        assert_eq!(mention_trigger_index("hi @fo"), Some(3));
        assert_eq!(mention_trigger_index("a@b"), None);
        assert_eq!(slash_trigger_index("/rev"), Some(0));
        assert_eq!(slash_trigger_index("x /rev"), Some(2));
        assert_eq!(slash_trigger_index("a/b"), None);
        assert_eq!(slash_trigger_index("/a b"), None);
    }

    #[test]
    fn currency() {
        assert_eq!(money(0.0123), "$0.01");
        assert_eq!(money(1234.5), "$1,234.50");
    }
}

