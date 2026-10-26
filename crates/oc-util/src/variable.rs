//! Config variable substitution (`core/src/config/variable.ts`).
//! Applied to raw text before JSONC parsing.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

pub struct Input<'a> {
    pub text: &'a str,
    /// Base directory for relative `{file:}` paths.
    pub dir: &'a Path,
    /// Source label (config file path or virtual label) used in warnings.
    pub source: &'a str,
    pub env: Option<&'a HashMap<String, String>>,
}

/// Apply `{env:VAR}` and `{file:path}` substitutions to config text.
///
/// A missing `{env:VAR}` and a missing or unreadable `{file:…}` both substitute empty (the latter
/// with a warning). `{env:}` values are JSON-escaped only when the raw values break the JSON (e.g.
/// Windows paths), so configs that parse today keep their meaning.
pub fn substitute(input: Input) -> String {
    let env_re = regex::Regex::new(r"\{env:([^}]+)\}").unwrap();
    let env = |escape: bool| -> String {
        env_re
            .replace_all(input.text, |c: &regex::Captures| {
                let name = &c[1];
                let value = input
                    .env
                    .and_then(|e| e.get(name).cloned())
                    .or_else(|| std::env::var(name).ok())
                    .unwrap_or_default();
                if escape {
                    let json = serde_json::to_string(&value).unwrap();
                    json[1..json.len() - 1].to_string()
                } else {
                    value
                }
            })
            .into_owned()
    };
    let raw = substitute_files(&input, &env(false));
    if !input.text.contains("{env:") || crate::jsonc::parse(&raw).is_ok() {
        return raw;
    }
    substitute_files(&input, &env(true))
}

fn substitute_files(input: &Input, text: &str) -> String {
    let file_re = regex::Regex::new(r"\{file:[^}]+\}").unwrap();
    let mut out = String::with_capacity(text.len());
    let mut last = 0;
    for m in file_re.find_iter(text) {
        out.push_str(&text[last..m.start()]);
        last = m.end();
        let token = m.as_str();
        let line_start = text[..m.start()].rfind('\n').map(|i| i + 1).unwrap_or(0);
        if text[line_start..m.start()].trim_start().starts_with("//") {
            out.push_str(token);
            continue;
        }
        let raw = &token["{file:".len()..token.len() - 1];
        let resolved: PathBuf = if let Some(rest) = raw.strip_prefix("~/") {
            dirs::home_dir().unwrap_or_default().join(rest)
        } else if Path::new(raw).is_absolute() {
            PathBuf::from(raw)
        } else {
            normalize(&input.dir.join(raw))
        };
        // Like a missing {env:VAR}, a missing or unreadable file substitutes empty so one stale secret
        // reference cannot take down every Location that loads this config.
        let content = match std::fs::read(&resolved) {
            Ok(bytes) => String::from_utf8_lossy(&bytes).into_owned(),
            Err(cause) => {
                tracing::warn!(
                    source = %input.source,
                    reference = %token,
                    path = %resolved.display(),
                    cause = %cause,
                    "config file reference is missing or unreadable"
                );
                String::new()
            }
        };
        let json = serde_json::to_string(js_trim(&content)).unwrap();
        out.push_str(&json[1..json.len() - 1]);
    }
    out.push_str(&text[last..]);
    out
}

/// `String.prototype.trim` (JS whitespace, which includes the BOM and line terminators).
fn js_trim(s: &str) -> &str {
    s.trim_matches(|c: char| c.is_whitespace() || c == '\u{feff}')
}

/// Lexical normalization like `path.resolve` (no symlink resolution).
pub fn normalize(p: &Path) -> PathBuf {
    use std::path::Component;
    let mut out = PathBuf::new();
    for c in p.components() {
        match c {
            Component::ParentDir => {
                out.pop();
            }
            Component::CurDir => {}
            other => out.push(other.as_os_str()),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(text: &str, env: &[(&str, &str)]) -> String {
        let env: HashMap<String, String> = env.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect();
        substitute(Input { text, dir: Path::new("/"), source: "test", env: Some(&env) })
    }

    // config.test.ts "keeps env values unescaped when the config already parses"
    #[test]
    fn keeps_env_values_unescaped_when_the_config_already_parses() {
        let text = run(
            r#"{ "quoted": "{env:QUOTED}", "fragment": {env:FRAGMENT} }"#,
            &[("QUOTED", r#"a\"b"#), ("FRAGMENT", r#"{"k":"v"}"#)],
        );
        assert_eq!(serde_json::from_str::<serde_json::Value>(&text).unwrap(), serde_json::json!({ "quoted": "a\"b", "fragment": { "k": "v" } }));
    }

    #[test]
    fn escapes_env_values_that_break_the_json() {
        let text = run(r#"{ "path": "{env:WIN}" }"#, &[("WIN", r#"C:\Users\new\"secret""#)]);
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&text).unwrap(),
            serde_json::json!({ "path": r#"C:\Users\new\"secret""# })
        );
    }

    #[test]
    fn missing_and_unreadable_files_substitute_empty() {
        let dir = std::env::temp_dir().join(format!("oc-variable-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let text = "{\n  // {file:nope}\n  \"a\": \"x{file:nope.txt}\", \"b\": \"{file:.}\"\n}";
        let out = substitute(Input { text, dir: &dir, source: "test", env: None });
        assert_eq!(out, "{\n  // {file:nope}\n  \"a\": \"x\", \"b\": \"\"\n}");
        std::fs::remove_dir_all(&dir).ok();
    }
}
